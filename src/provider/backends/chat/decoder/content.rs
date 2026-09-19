//! Assemble visible text, reasoning envelopes, and sequential tool fragments.
use super::super::wire;
use super::{Block, Decoder};
use crate::provider::{
    ProviderError,
    backends::common::{self, parse_tool_arguments, reasoning_envelope},
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
        if let Some(text) = delta.reasoning_text() {
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
                start_item(chunks, id, ItemKind::ToolCall, BlockKind::ToolCallArguments);
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
                    chunks.push(ResponseChunk::BlockDelta {
                        item: id.to_string(),
                        block: "0".into(),
                        delta: ContentDelta::JsonFragment(fragment.clone()),
                    });
                }
            }
        }
        Ok(())
    }

    /// The part of a full message not already streamed. A message that
    /// repeats or extends the stream adds only what is new; one that differs
    /// (trimmed, reformatted) leaves the streamed content as it is.
    pub(super) fn unstreamed(&self, message: wire::Delta) -> wire::Delta {
        let streamed = |reasoning: bool| -> String {
            self.blocks
                .iter()
                .filter_map(|block| match (block, reasoning) {
                    (Block::Text(text), false) | (Block::Reasoning(text), true) => {
                        Some(text.as_str())
                    }
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
        let mut missing = wire::Delta {
            reasoning_content: remainder(streamed(true), message.reasoning_text()),
            content: remainder(streamed(false), Some(&visible)),
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
                    BlockContent::ToolCall(
                        ToolCall::new(call_id, name.clone(), Value::Object(arguments))
                            .map_err(|error| ProviderError::protocol(format!("Chat: {error}")))?,
                    )
                }
            };
            self.end_item(chunks, id, block);
        }
        Ok(())
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
                        Block::Reasoning(text) => (b"r", [text, "", ""]),
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

fn start_item(chunks: &mut Vec<ResponseChunk>, id: usize, kind: ItemKind, block_kind: BlockKind) {
    chunks.extend(common::start_item(id, kind, block_kind));
}

impl Decoder {
    fn end_item(&self, chunks: &mut Vec<ResponseChunk>, id: usize, content: BlockContent) {
        // The transport wrapper binds the provider+endpoint scope.
        let replay = match &content {
            BlockContent::Reasoning { text } => Some(reasoning_envelope(
                "chat_completions",
                &self.model,
                json!({"text": text}),
            )),
            _ => None,
        };
        chunks.extend(common::end_item(id, content, replay));
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Decoder, tests::*};
    use crate::provider::backends::transport::SseEvent;
    use crate::provider::protocol::{
        AssistantItem, BlockContent, ContentDelta, ItemKind, ResponseAssembler, ResponseChunk,
        StopReason, ToolCall, Usage,
    };
    use serde_json::{Value, json};
    use std::collections::BTreeSet;

    fn reasoning(text: &str) -> BlockContent {
        BlockContent::Reasoning { text: text.into() }
    }

    fn tool(id: &str, name: &str, arguments: Value) -> BlockContent {
        BlockContent::ToolCall(ToolCall::new(id, name, arguments).unwrap())
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
        let calls: Vec<_> = items
            .iter()
            .filter_map(|item| match &item.blocks[0].content {
                BlockContent::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect();
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
        let (items, _, stop) = decode(frames);
        assert_eq!(stop, StopReason::ToolUse);
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(id, name, arguments)| (id.to_owned(), name.to_owned(), arguments))
            .collect();
        assert_eq!(calls_of(&items), expected);
    }

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
        let (items, usage, stop) = decode(frames);
        assert_eq!(stop, StopReason::ToolUse);
        assert_eq!((usage.input_tokens, usage.output_tokens), (447, 84));
        let expected = [
            reasoning("Compare."),
            text("399 is larger."),
            tool("tooluse_1", "run", json!({"cmd":"ls"})),
            tool("tooluse_2", "list_jobs", json!({})),
        ];
        assert_eq!(contents(&items), expected.iter().collect::<Vec<_>>());
    }

    #[test]
    fn unknown_and_malformed_delta_fields_are_ignored() {
        // Unknown fields, and known ones with unusable shapes.
        for field in [
            "unknown",
            "audio",
            "provider_specific_fields",
            "thinking_blocks",
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
                assert_eq!(contents(&items), [&text("answer")]);
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
            let mut decoder = Decoder::new("test-model".into()).for_request(&request);
            let mut assembler = ResponseAssembler::default();
            for frame in [event(chunk), end("tool_calls"), done()] {
                for chunk in decoder.decode(&frame).unwrap() {
                    assembler.push(&chunk).unwrap();
                }
            }
            let (items, _, _) = assembler.finish().unwrap();
            match &items[0].blocks[0].content {
                BlockContent::ToolCall(call) => call.id().to_owned(),
                _ => unreachable!(),
            }
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
        assert_eq!(contents(&items), [&reasoning("primary"), &text("answer")]);
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

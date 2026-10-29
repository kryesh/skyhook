//! Native reasoning objects: thinking blocks and reasoning details, kept whole
//! for replay beside the text they carry.
use super::super::wire;
use super::{Block, Decoder, NativeShape};
use crate::provider::{
    codec::common::{self, replay},
    protocol::{Binding, ItemKind, Replay, ReplayFormat, ResponseEvent},
};
use serde_json::{Map, Value};

impl Decoder {
    /// A `thinking_blocks` entry. Thinking fragments continue the block
    /// receiving output, and a completing entry completes the latest open block
    /// whatever output came since; redacted thinking stands alone. A completing
    /// entry re-sends either the block's text so far (a snapshot, of which only
    /// the unstreamed part is new) or an empty marker; any other text it carries
    /// is a fragment.
    pub(super) fn thinking_block(
        &mut self,
        block: &wire::Native<wire::ThinkingBlock>,
        events: &mut Vec<ResponseEvent>,
    ) {
        let signed = block.view.is_signed();
        let open = |block: &Block| {
            matches!(
                block,
                Block::Native {
                    shape: NativeShape::Thinking {
                        complete: false,
                        ..
                    },
                    ..
                }
            )
        };
        let complete = block.view.completes();
        let (open, shape) = match block.view {
            wire::ThinkingBlock::Other => return,
            wire::ThinkingBlock::Thinking { .. } if complete => (
                self.blocks.iter().rposition(open),
                NativeShape::Thinking { complete, signed },
            ),
            wire::ThinkingBlock::Thinking { .. } => (
                self.blocks
                    .len()
                    .checked_sub(1)
                    .filter(|id| open(&self.blocks[*id])),
                NativeShape::Thinking { complete, signed },
            ),
            wire::ThinkingBlock::Redacted { .. } => (None, NativeShape::Redacted { signed }),
        };
        let fragment = block.view.text().unwrap_or("");
        if open.is_none() && fragment.is_empty() && !complete {
            return;
        }
        let id = open.unwrap_or_else(|| self.open_native(shape));
        let Block::Native {
            object,
            shape: current,
        } = &mut self.blocks[id]
        else {
            unreachable!()
        };
        let new = if complete {
            fragment
                .strip_prefix(current.text(object))
                .unwrap_or(fragment)
        } else {
            fragment
        };
        *current = shape;
        self.native_fragment(id, new, &block.raw, events);
    }

    /// A `reasoning_details` entry, continuing an unsealed detail it fits. Its text
    /// continues the latest detail at its index among the native objects receiving
    /// output; a signature alone seals the latest it fits, whatever output came
    /// since. Every entry is replayed, even an empty one.
    pub(super) fn detail(
        &mut self,
        detail: &wire::Native<wire::Detail>,
        events: &mut Vec<ResponseEvent>,
    ) {
        let view = &detail.view;
        let fits = |block: &Block| match block {
            Block::Native {
                object,
                shape:
                    NativeShape::Detail {
                        index,
                        kind,
                        signed: false,
                    },
            } => view.continues(*index, *kind, object.get("id").and_then(Value::as_str)),
            _ => false,
        };
        let open = if view.signed && view.text.as_deref().is_none_or(str::is_empty) {
            self.blocks.iter().rposition(fits)
        } else {
            let run = self
                .blocks
                .iter()
                .rposition(|block| !matches!(block, Block::Native { .. }))
                .map_or(0, |id| id + 1);
            let at_index = |block: &Block| {
                matches!(block, Block::Native {
                    shape: NativeShape::Detail { index, .. },
                    ..
                } if view.index.is_none() || *index == view.index)
            };
            let latest = self.blocks[run..].iter().rposition(at_index);
            latest
                .map(|id| run + id)
                .filter(|id| fits(&self.blocks[*id]))
        };
        let id = open.unwrap_or_else(|| {
            self.open_native(NativeShape::Detail {
                index: view.index,
                kind: view.kind,
                signed: false,
            })
        });
        let Block::Native {
            shape: NativeShape::Detail { signed, .. },
            ..
        } = &mut self.blocks[id]
        else {
            unreachable!()
        };
        *signed |= view.signed;
        let new = view.text.as_deref().unwrap_or("");
        self.native_fragment(id, new, &detail.raw, events);
    }

    /// Whether this response carries native reasoning with readable text, which
    /// then takes the place of its plain reasoning.
    pub(super) fn reads_native(&self) -> bool {
        self.blocks.iter().any(|block| {
            matches!(block, Block::Native { object, shape } if !shape.text(object).is_empty())
        })
    }

    fn open_native(&mut self, shape: NativeShape) -> usize {
        let id = self.blocks.len();
        self.blocks.push(Block::Native {
            object: Map::new(),
            shape,
        });
        id
    }

    /// Merge a fragment's fields into its native object (later values win),
    /// except for the text so far: a text fragment extends it by `new`, and a
    /// fragment without text leaves it as it is.
    fn native_fragment(
        &mut self,
        id: usize,
        new: &str,
        raw: &Map<String, Value>,
        events: &mut Vec<ResponseEvent>,
    ) {
        let Block::Native { object, shape } = &mut self.blocks[id] else {
            unreachable!()
        };
        for (name, value) in raw {
            match object.get_mut(name) {
                Some(Value::String(text)) if Some(name.as_str()) == shape.text_field() => {
                    if value.is_string() {
                        text.push_str(new);
                    }
                }
                _ => {
                    object.insert(name.clone(), value.clone());
                }
            }
        }
        if !new.is_empty() {
            events.push(common::delta(id, ItemKind::Reasoning, new));
        }
    }

    /// Whether this completion's details are signed. Details replay as one
    /// sequence, so one signed detail binds them all.
    pub(super) fn details_signed(&self) -> bool {
        self.blocks.iter().any(|block| {
            matches!(
                block,
                Block::Native {
                    shape: NativeShape::Detail { signed: true, .. },
                    ..
                }
            )
        })
    }

    /// A native object's replay: the object whole. A signed object is bound to
    /// the exact conversation before it, like Messages thinking; an unsigned one
    /// replays freely.
    pub(super) fn native_replay(
        &self,
        object: &Map<String, Value>,
        shape: NativeShape,
        details_signed: bool,
    ) -> Replay {
        let (format, signed) = match shape {
            NativeShape::Thinking { signed, .. } | NativeShape::Redacted { signed } => {
                (ReplayFormat::ChatThinkingBlock, signed)
            }
            NativeShape::Detail { .. } => (ReplayFormat::ChatReasoningDetail, details_signed),
        };
        let binding = if signed {
            Binding::Conversation
        } else {
            Binding::Free
        };
        replay(
            format,
            &self.model,
            &self.scope,
            Value::Object(object.clone()),
            binding,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::ReasoningFormat;
    use super::super::tests::*;
    use crate::provider::codec::common::tests::Reduced;
    use crate::provider::protocol::{AssistantItem, Binding, ItemKind, Outcome};
    use serde_json::{Value, json};

    /// The payload and binding of each reasoning item, all of `format`.
    fn native_replays(reduced: &Reduced, format: ReasoningFormat) -> Vec<(Value, Binding)> {
        reduced
            .items()
            .iter()
            .filter_map(AssistantItem::replay)
            .map(|replay| {
                assert_eq!(replay.provenance.format, format.replay());
                (replay.payload.clone(), replay.binding)
            })
            .collect()
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
        assert_eq!(
            (usage.input_tokens.unwrap(), usage.output_tokens.unwrap()),
            (447, 84)
        );
        let expected = [
            reasoning("Compare."),
            text("399 is larger."),
            tool("tooluse_1", "run", json!({"cmd":"ls"})),
            tool("tooluse_2", "list_jobs", json!({})),
        ];
        assert_eq!(contents(&items), expected);
        let text_replay = items[0].replay().unwrap();
        assert_eq!(
            text_replay.provenance.format,
            ReasoningFormat::Text.replay()
        );
        assert_eq!(text_replay.payload, json!({"text":"Compare."}));
        // A dialect replaying thinking blocks keeps each signed block whole and
        // binds it to the conversation. An empty completing marker keeps the
        // text streamed before it. An unknown or malformed entry leaves the
        // open block as it is.
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
        let reduced = reduced_as(ReasoningFormat::ThinkingBlocks, frames);
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
            native_replays(&reduced, ReasoningFormat::ThinkingBlocks),
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

    #[test]
    fn a_signature_entry_resending_the_block_text_adds_none() {
        // Each thinking delta carries its fragment, and the signature delta
        // re-sends the block's text so far.
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
        let reduced = reduced_as(ReasoningFormat::ThinkingBlocks, frames.clone());
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
            native_replays(&reduced, ReasoningFormat::ThinkingBlocks),
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
    fn a_fragment_without_text_keeps_the_text_so_far() {
        for (format, field, text_field, base) in [
            (
                ReasoningFormat::Details,
                "reasoning_details",
                "text",
                json!({"type":"reasoning.text","index":0}),
            ),
            (
                ReasoningFormat::ThinkingBlocks,
                "thinking_blocks",
                "thinking",
                json!({"type":"thinking"}),
            ),
        ] {
            let entry = |text: Value, signature: Value| {
                let mut entry = base.clone();
                entry[text_field] = text;
                entry["signature"] = signature;
                entry
            };
            let fragment =
                |text: Value, signature: Value| delta(json!({ field: [entry(text, signature)] }));
            let reduced = reduced_as(
                format,
                vec![
                    fragment(json!("Weigh "), Value::Null),
                    fragment(Value::Null, Value::Null),
                    fragment(json!("both."), Value::Null),
                    fragment(Value::Null, json!("sig")),
                    delta(json!({"content":"399."})),
                    end("stop"),
                ],
            );
            assert_eq!(reduced.streamed(ItemKind::Reasoning), ["Weigh both."]);
            assert_eq!(
                contents(reduced.items()),
                [reasoning("Weigh both."), text("399.")]
            );
            assert_eq!(
                native_replays(&reduced, format),
                [(
                    entry(json!("Weigh both."), json!("sig")),
                    Binding::Conversation
                )]
            );
        }
    }

    #[test]
    fn native_objects_repeated_by_the_final_message_are_kept_once() {
        let redacted = json!({"type":"redacted_thinking","data":"opaque"});
        let reduced = reduced_as(
            ReasoningFormat::ThinkingBlocks,
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
        let reduced = reduced_as(
            ReasoningFormat::ThinkingBlocks,
            vec![event(
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop",
                "message":{"content":"answer","thinking_blocks":[redacted]}}]}),
            )],
        );
        assert_eq!(
            native_replays(&reduced, ReasoningFormat::ThinkingBlocks),
            [(redacted, Binding::Conversation)]
        );
    }

    #[test]
    fn redacted_thinking_has_no_readable_text() {
        let redacted =
            json!({"type":"redacted_thinking","data":"opaque","thinking":"not-display-text"});
        let message = json!({"content":"answer","thinking_blocks":[redacted]});
        for frames in [
            vec![
                delta(json!({"thinking_blocks":[redacted]})),
                delta(json!({"content":"answer"})),
                end("stop"),
            ],
            vec![event(
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop","message":message}]}),
            )],
        ] {
            let reduced = reduced_as(ReasoningFormat::ThinkingBlocks, frames);
            assert!(reduced.streamed(ItemKind::Reasoning).is_empty());
            assert_eq!(contents(reduced.items()), [reasoning(""), text("answer")]);
            assert_eq!(
                native_replays(&reduced, ReasoningFormat::ThinkingBlocks),
                [(redacted.clone(), Binding::Conversation)]
            );
        }
    }

    #[test]
    fn empty_native_arrays_keep_the_reasoning_text() {
        for replay in [ReasoningFormat::ThinkingBlocks, ReasoningFormat::Details] {
            let reduced = reduced_as(
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
        for replay in [ReasoningFormat::Text, ReasoningFormat::Details] {
            let reduced = reduced_as(
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
        let reduced = reduced_as(ReasoningFormat::Details, frames.clone());
        assert_eq!(
            contents(reduced.items()),
            [reasoning("Weigh both."), reasoning(""), text("399.")]
        );
        let bound = Binding::Conversation;
        assert_eq!(
            native_replays(&reduced, ReasoningFormat::Details),
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
            let reduced = reduced_as(
                ReasoningFormat::Details,
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
            assert_eq!(native_replays(&reduced, ReasoningFormat::Details), expected);
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
    fn a_new_type_or_id_at_a_reused_index_starts_the_next_detail() {
        // OpenRouter's ai-sdk-provider issue-519 fixture: every entry at index 0.
        let summary = |text: &str| json!({"type":"reasoning.summary","summary":text,"format":"openai-responses-v1","index":0});
        let encrypted = |data: &str, id: &str| json!({"type":"reasoning.encrypted","data":data,"id":id,"index":0});
        let frames = vec![
            delta(json!({"role":"assistant","content":"","reasoning_details":[summary("a")]})),
            delta(json!({"reasoning_details":[summary("b")]})),
            delta(json!({"reasoning_details":[encrypted("BLOB1", "rs_1")]})),
            delta(json!({"reasoning_details":[summary("c")]})),
            delta(json!({"reasoning_details":[encrypted("BLOB2", "rs_2")]})),
            delta(json!({"content":"done"})),
            end("stop"),
        ];
        let reduced = reduced_as(ReasoningFormat::Details, frames);
        assert_eq!(
            contents(reduced.items()),
            [
                reasoning("ab"),
                reasoning(""),
                reasoning("c"),
                reasoning(""),
                text("done"),
            ]
        );
        let details = [
            summary("ab"),
            encrypted("BLOB1", "rs_1"),
            summary("c"),
            encrypted("BLOB2", "rs_2"),
        ];
        let bound = details
            .iter()
            .map(|detail| (detail.clone(), Binding::Conversation));
        assert_eq!(
            native_replays(&reduced, ReasoningFormat::Details),
            bound.collect::<Vec<_>>()
        );
        use super::super::super::{ReasoningReplay, encode, encoder::tests::*};
        let mut request = crate::provider::codec::common::tests::request("test-model");
        let turn = crate::provider::protocol::Message::Assistant(reduced.items().to_vec());
        (request.context).extend(crate::provider::protocol::entries([turn]));
        let router = native_dialect(ReasoningReplay::Details, "reasoning_details", replayed());
        let body = encode(&request, Default::default(), &router).unwrap();
        assert_eq!(body["messages"][1]["reasoning_details"], json!(details));
        // So does another id on a detail of the same type; an encrypted entry is
        // whole, so one without an id never continues another.
        let null_id =
            |data: &str| json!({"type":"reasoning.encrypted","data":data,"id":null,"index":0});
        for blobs in [
            [encrypted("BLOB1", "rs_1"), encrypted("BLOB2", "rs_2")],
            [null_id("BLOB1"), null_id("BLOB2")],
        ] {
            let reduced = reduced_as(
                ReasoningFormat::Details,
                vec![delta(json!({ "reasoning_details": blobs })), end("stop")],
            );
            let replays = native_replays(&reduced, ReasoningFormat::Details);
            let payloads: Vec<_> = replays.into_iter().map(|(payload, _)| payload).collect();
            assert_eq!(payloads, blobs);
        }
        // While native objects receive output, an indexed fragment continues the
        // latest detail at its index.
        let entry = |kind: &str, field: &str, text: &str, index: u64| json!({"type": format!("reasoning.{kind}"), field: text, "index": index});
        let reduced = reduced_as(
            ReasoningFormat::Details,
            vec![
                delta(json!({"reasoning_details":[entry("text", "text", "a", 0),
                    entry("summary", "summary", "s", 1), entry("text", "text", "b", 0)]})),
                delta(json!({"reasoning_details":[entry("summary", "summary", "t", 1)]})),
                end("stop"),
            ],
        );
        assert_eq!(
            contents(reduced.items()),
            [reasoning("ab"), reasoning("st")]
        );
        // Readable fragments without an index continue the latest detail of their type.
        let text = |text: &str| {
            delta(json!({"reasoning_details":[{"type":"reasoning.text","text":text}]}))
        };
        let reduced = reduced_as(
            ReasoningFormat::Details,
            vec![
                text("Weigh "),
                text("both."),
                delta(json!({"reasoning_details":[{"type":"reasoning.summary","summary":"Sum"}]})),
                text("Again"),
                end("stop"),
            ],
        );
        assert_eq!(
            contents(reduced.items()),
            [
                reasoning("Weigh both."),
                reasoning("Sum"),
                reasoning("Again")
            ]
        );
    }

    /// A response's readable details take the place of its plain reasoning, which a
    /// provider may stream and then send whole as a detail; it is shown live and
    /// kept only where the response has no readable detail.
    #[test]
    fn readable_details_take_the_place_of_plain_reasoning_per_response() {
        let signed = json!({"type":"reasoning.text","text":"Weigh both.","signature":"sig"});
        let reduced = reduced_as(
            ReasoningFormat::Details,
            vec![
                delta(json!({"reasoning":"Weigh "})),
                delta(json!({"reasoning":"both."})),
                delta(json!({"reasoning":"Weigh both.","reasoning_details":[signed]})),
                delta(json!({"reasoning":"Later."})),
                delta(json!({"content":"399."})),
                end("stop"),
            ],
        );
        assert_eq!(
            reduced.streamed(ItemKind::Reasoning),
            ["Weigh both.", "Weigh both."]
        );
        assert_eq!(
            contents(reduced.items()),
            [reasoning("Weigh both."), text("399.")]
        );
        assert_eq!(
            native_replays(&reduced, ReasoningFormat::Details),
            [(signed, Binding::Conversation)]
        );
        // Without readable details, the plain reasoning stays beside them.
        let opaque = json!({"type":"reasoning.encrypted","data":""});
        let reduced = reduced_as(
            ReasoningFormat::Details,
            vec![
                delta(json!({"reasoning":"Weigh.","reasoning_details":[opaque]})),
                delta(json!({"content":"399."})),
                end("stop"),
            ],
        );
        assert_eq!(
            contents(reduced.items()),
            [reasoning(""), reasoning("Weigh."), text("399.")]
        );
    }

    /// Text continues a detail only while it receives output and before its
    /// signature; a signature alone seals the latest open detail across output since.
    /// Thinking blocks complete the same way. Fragments may carry no index or reuse one.
    #[test]
    fn a_signed_detail_is_never_extended() {
        let call = |index: u64, id: &str| {
            delta(
                json!({"tool_calls":[{"index":index,"id":id,"function":{"name":"run","arguments":"{}"}}]}),
            )
        };
        let content = |text: &str| delta(json!({ "content": text }));
        for index in [Value::Null, json!(0)] {
            let detail = |text: &str, signature: Option<&str>| {
                let mut detail = json!({"type":"reasoning.text","text":text,"index":index});
                if let Some(signature) = signature {
                    detail["signature"] = json!(signature);
                }
                delta(json!({ "reasoning_details": [detail] }))
            };
            let reduced = reduced_as(
                ReasoningFormat::Details,
                vec![
                    detail("First.", None),
                    detail("", Some("s1")),
                    call(0, "c1"),
                    detail("Second.", None),
                    detail("", Some("s2")),
                    detail("Third.", None),
                    content("ok"),
                    detail("Fourth.", None),
                    content("more"),
                    detail("", Some("s4")),
                    detail("Fifth.", None),
                    call(1, "c2"),
                    detail("", Some("s5")),
                    end("stop"),
                ],
            );
            let texts: Vec<_> = native_replays(&reduced, ReasoningFormat::Details)
                .into_iter()
                .map(|(payload, _)| (payload["text"].clone(), payload.get("signature").cloned()))
                .collect();
            assert_eq!(
                texts,
                [
                    (json!("First."), Some(json!("s1"))),
                    (json!("Second."), Some(json!("s2"))),
                    (json!("Third."), None),
                    (json!("Fourth."), Some(json!("s4"))),
                    (json!("Fifth."), Some(json!("s5"))),
                ],
                "{index}"
            );
        }
        let thinking =
            |text: &str| delta(json!({"thinking_blocks":[{"type":"thinking","thinking":text}]}));
        let sealed =
            delta(json!({"thinking_blocks":[{"type":"thinking","thinking":"","signature":"sig"}]}));
        for between in [content("Hi"), call(0, "c1")] {
            let reduced = reduced_as(
                ReasoningFormat::ThinkingBlocks,
                vec![thinking("Think."), between, sealed.clone(), end("stop")],
            );
            assert_eq!(
                native_replays(&reduced, ReasoningFormat::ThinkingBlocks),
                [(
                    json!({"type":"thinking","thinking":"Think.","signature":"sig"}),
                    Binding::Conversation
                )]
            );
        }
    }
}

//! Encode Responses input history, replay, runtime state, and explicit cache checkpoints.
use super::{Dialect, ProviderError, native::ItemType, reasoning_parts};
use crate::media::Image;
use crate::provider::{
    ProviderErrorKind::InvalidRequest,
    codec::{
        Cache,
        common::{
            PreparedImages, StateWire, image_url, is_sealed, own_replay, push_state,
            signed_context, tagged, tool_text, user_parts,
        },
        placement::{explicit_breakpoint, push_history},
    },
    protocol::{AssistantItem, Entry, Message, ModelRequest, Replay, ReplayFormat},
};
use serde_json::{Value, json};

pub(super) fn encode(
    request: &ModelRequest,
    images: PreparedImages<'_>,
    dialect: &Dialect,
) -> Result<Vec<Value>, ProviderError> {
    let mut input = Input::default();
    let budget = match dialect.cache {
        Cache::Explicit => Some(dialect.breakpoints),
        Cache::Automatic => None,
    };
    let unsigned =
        dialect.unsigned.replayed().is_some() && !signed_context(request, ReplayFormat::Responses);
    // Current state may coalesce into the prefix tail's wrapper, but not its marked text block.
    push_history(
        &mut input,
        request,
        budget,
        0,
        |input, entry| input.push_entry(request, images, dialect, unsigned, entry),
        |input| input.text,
        Input::mark,
    )?;
    Ok(input.items)
}

const STATE: StateWire = StateWire {
    system: "developer",
    text: "input_text",
    message: |role, content| json!({"type":"message", "role":role, "content":content}),
};

/// Input items, and the last nonempty text part among them: where an explicit cache
/// checkpoint goes.
#[derive(Default)]
struct Input {
    items: Vec<Value>,
    text: Option<TextPart>,
}

/// A text part: its item, the item's field holding its parts, and its index there.
#[derive(Clone, Copy, PartialEq)]
struct TextPart(usize, &'static str, usize);

impl Input {
    /// Push `item`, whose `field` may hold a nonempty text part at an index.
    fn push(&mut self, item: Value, text: Option<(&'static str, usize)>) {
        if let Some((field, part)) = text {
            self.text = Some(TextPart(self.items.len(), field, part));
        }
        self.items.push(item);
    }

    fn mark(&mut self, TextPart(item, field, part): TextPart) {
        self.items[item][field][part]["prompt_cache_breakpoint"] = explicit_breakpoint();
    }

    fn push_entry(
        &mut self,
        request: &ModelRequest,
        images: PreparedImages<'_>,
        dialect: &Dialect,
        unsigned: bool,
        entry: &Entry,
    ) -> Result<(), ProviderError> {
        let image_part =
            |image: &Image| json!({"type":"input_image", "image_url":image_url(image)});
        let message = match entry {
            Entry::Message(message) => message,
            Entry::State(state) => {
                let pushed = push_state(dialect.state_role, &STATE, &mut self.items, state);
                if let Some((item, part)) = pushed
                    && !state.text.is_empty()
                {
                    self.text = Some(TextPart(item, "content", part));
                }
                return Ok(());
            }
        };
        match message {
            Message::User(parts) => {
                let content = user_parts(request, images, parts, "input_text", image_part)?;
                let text = content.iter().rposition(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty())
                });
                self.push(
                    json!({"type":"message", "role":"user", "content":content}),
                    text.map(|part| ("content", part)),
                );
            }
            Message::Assistant(items) => {
                for item in items {
                    match item {
                        // Private replay belongs to the item, never to each display summary.
                        AssistantItem::Reasoning { replay, .. } => {
                            if let Some(Replay {
                                payload: native, ..
                            }) = own_replay(
                                replay.as_ref(),
                                ReplayFormat::Responses,
                                request.model.as_str(),
                            )
                            .filter(|replay| unsigned || is_sealed(replay))
                            {
                                if tagged(native) != Some(ItemType::Reasoning)
                                    || reasoning_parts(native).is_err()
                                {
                                    return Err(InvalidRequest.error(
                                        "Responses reasoning replay is not a readable reasoning item",
                                    ));
                                }
                                self.push(native.clone(), None);
                            }
                        }
                        // `type` is optional in the spec, but some servers
                        // cannot identify an assistant item without it.
                        AssistantItem::Text { blocks, .. } => {
                            for block in blocks {
                                self.push(
                                    json!({
                                        "type":"message", "role":"assistant",
                                        "content":[{"type":"output_text", "text":block.text}]
                                    }),
                                    None,
                                );
                            }
                        }
                        AssistantItem::ToolCall { call, .. } => {
                            self.push(json!({"type":"function_call", "call_id":call.id(),
                                    "name":call.name(), "arguments":serde_json::to_string(call.arguments()).expect("JSON object serialization cannot fail")}), None);
                        }
                    }
                }
            }
            Message::Tool(results) => {
                for result in results {
                    if result.call_id.is_empty() {
                        return Err(
                            InvalidRequest.error("Responses tool output requires a call ID")
                        );
                    }
                    let (output, text) = match dialect.cache {
                        Cache::Automatic => (json!(tool_text(result)), None),
                        Cache::Explicit => (
                            json!([{"type":"input_text", "text":tool_text(result)}]),
                            Some(("output", 0)),
                        ),
                    };
                    self.push(
                        json!({"type":"function_call_output",
                            "call_id":result.call_id, "output":output}),
                        text,
                    );
                    if !result.images.is_empty() {
                        let mut content = vec![json!({"type":"input_text", "text":format!(
                                "Images from tool {} (call_id: {}):", result.name, result.call_id)})];
                        for image in &result.images {
                            content.push(images.part(image, "input_text", image_part)?);
                        }
                        self.push(
                            json!({"type":"message", "role":"user", "content":content}),
                            Some(("content", 0)),
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        assemble_with, call_item, completed, decoder_for, message, reasoning_item,
    };
    use super::*;
    use crate::media::AttachmentRef;
    use crate::provider::codec::StateRole;
    use crate::provider::{
        codec::common::tests::{
            envelope, image, image_data, loaded, notes, request, tool_result, user,
        },
        protocol::{Binding, HistoryLifetime, Outcome, UserContent, entries},
    };

    fn assemble(output: Vec<Value>) -> Vec<AssistantItem> {
        let reduced = assemble_with(decoder_for("gpt-5"), vec![completed(output)]).unwrap();
        reduced.completion.items().to_vec()
    }

    #[tokio::test]
    async fn user_attachments_and_tool_images_require_loaded_blobs() {
        let mut req = request("gpt-5");
        let attach = |attachment| UserContent::Attachment { attachment };
        let Message::User(mut content) = user("look") else {
            unreachable!()
        };
        content.extend([
            attach(AttachmentRef::Image(image())),
            attach(AttachmentRef::Text(notes())),
        ]);
        req.context = entries(vec![
            Message::User(content),
            tool_result(
                "call_1",
                "search",
                json!({"answer":42, "error":null}),
                vec![image()],
                true,
            ),
        ]);
        assert!(encode(&req, Default::default(), &Dialect::stateless()).is_err());
        let images = loaded(&mut req).await;
        let input = encode(&req, images.prepared(), &Dialect::stateless()).unwrap();
        let image_url = image_data();
        let content = &input[0]["content"];
        assert_eq!(content[0]["text"], "look");
        assert_eq!(content[1]["image_url"], image_url);
        assert_eq!(
            content[2],
            json!({"type":"input_text", "text":"File: notes.txt\nnotes"})
        );
        assert_eq!(input[1]["call_id"], "call_1");
        let result: Value = serde_json::from_str(input[1]["output"].as_str().unwrap()).unwrap();
        assert_eq!(
            result,
            json!({"result":{"answer":42,"error":null},"is_error":true})
        );
        assert_eq!(input[2]["content"][1]["image_url"], image_url);
    }

    #[test]
    fn reasoning_from_other_providers_or_models_is_not_replayed() {
        for (format, model) in [
            (ReplayFormat::Messages, "gpt-5"),
            (ReplayFormat::Responses, "other-model"),
        ] {
            let mut req = request("gpt-5");
            let replay = envelope(format, model, reasoning_item(), Binding::Free);
            let reasoning = AssistantItem::reasoning("r", 0, "private", Some(replay));
            req.context = entries(vec![Message::Assistant(vec![reasoning])]);
            assert_eq!(
                encode(&req, Default::default(), &Dialect::stateless()).unwrap(),
                Vec::<Value>::new()
            );
        }
    }

    /// Unencrypted reasoning goes back until the model encrypts reasoning in the
    /// context, and never where the dialect takes only encrypted reasoning.
    #[test]
    fn unencrypted_reasoning_replays_only_before_encrypted_reasoning() {
        let sealed =
            json!({"type":"reasoning", "id":"rs_1", "summary":[], "encrypted_content":"BLOB"});
        let plain = json!({"type":"reasoning", "id":"rs_2",
            "summary":[{"type":"summary_text", "text":"plain"}]});
        let turn = |native: &Value| {
            let replay = envelope(
                ReplayFormat::Responses,
                "test-model",
                native.clone(),
                Binding::Free,
            );
            Message::Assistant(vec![AssistantItem::reasoning("r", 0, "", Some(replay))])
        };
        let sealed_only = Dialect {
            unsigned: crate::provider::codec::UnsignedReasoning::Dropped,
            ..Dialect::stateless()
        };
        for (turns, dialect, expected) in [
            (vec![&plain], Dialect::stateless(), vec![&plain]),
            (vec![&sealed, &plain], Dialect::stateless(), vec![&sealed]),
            (vec![&plain, &sealed], sealed_only, vec![&sealed]),
        ] {
            let mut req = request("test-model");
            req.context = entries(turns.into_iter().map(turn));
            let input = encode(&req, Default::default(), &dialect).unwrap();
            assert_eq!(input.iter().collect::<Vec<_>>(), expected);
        }
    }

    #[test]
    fn encrypted_reasoning_replays_only_to_its_model_beside_tools() {
        let mut req = request("gpt-5");
        // No display summary is required for native reasoning to be replayable.
        let native = json!({"type":"reasoning", "id":"rs_opaque", "summary":[],
            "encrypted_content":"opaque+/=", "future_state":{"signature":"unchanged"},
            "content":[{"type":"reasoning_text", "text":"native reasoning text"}]});
        let output = vec![completed(vec![native.clone(), call_item()])];
        let reduced = assemble_with(decoder_for("gpt-5"), output).unwrap();
        assert_eq!(reduced.completion.outcome(), Outcome::ToolUse);
        let items = reduced.items().to_vec();
        assert_eq!(
            items[0].reasoning_text().as_deref(),
            Some("native reasoning text")
        );
        req.context = entries(vec![
            Message::Assistant(items),
            tool_result("call_1", "search", json!({"found":true}), vec![], false),
        ]);
        let input = encode(&req, Default::default(), &Dialect::stateless()).unwrap();
        assert_eq!(input[0], native);
        assert_eq!(
            (&input[1]["type"], &input[2]["type"]),
            (&json!("function_call"), &json!("function_call_output"))
        );
        assert_eq!(input[1]["call_id"], input[2]["call_id"]);
        // Another model's reasoning stays behind; the call and its output do not.
        req.model = "gpt-6".parse().unwrap();
        let wire = encode(&req, Default::default(), &Dialect::stateless()).unwrap();
        assert_eq!(wire, vec![input[1].clone(), input[2].clone()]);
    }

    #[test]
    fn terminal_only_output_replays_across_tool_and_followup_turns() {
        let mut native_reasoning = reasoning_item();
        native_reasoning["encrypted_content"] = json!("mock-replay-state");
        let items = assemble(vec![
            native_reasoning.clone(),
            message("text-terminal", "checking"),
            call_item(),
        ]);
        let mut req = request("gpt-5");
        req.context = entries(vec![
            user("look it up"),
            Message::Assistant(items),
            tool_result("call_1", "search", json!({"value":42}), vec![], false),
        ]);
        let input = encode(&req, Default::default(), &Dialect::stateless()).unwrap();
        let reasoning = input.iter().filter(|item| item["type"] == "reasoning");
        assert_eq!(reasoning.count(), 1);
        assert_eq!(input[1], native_reasoning);
        // Assistant items are typed: some servers cannot identify them otherwise.
        assert_eq!(
            (&input[2]["type"], &input[2]["role"]),
            (&json!("message"), &json!("assistant"))
        );
        assert_eq!(
            input[2]["content"][0],
            json!({"type":"output_text", "text":"checking"})
        );
        assert_eq!(
            input[3],
            json!({"type":"function_call", "call_id":"call_1", "name":"search", "arguments":"{\"query\":\"rust\"}"})
        );
        assert_eq!(
            (&input[4]["type"], &input[4]["call_id"]),
            (&json!("function_call_output"), &json!("call_1"))
        );

        req.context
            .push(Entry::from(Message::Assistant(assemble(vec![message(
                "answer", "42",
            )]))));
        req.context.push(Entry::from(user("thanks")));
        let replayed = encode(&req, Default::default(), &Dialect::stateless()).unwrap();
        assert_eq!(&replayed[..input.len()], input.as_slice());
        assert_eq!(replayed[5]["content"][0]["text"], "42");
        assert_eq!(replayed[6]["content"][0]["text"], "thanks");
    }

    /// A state in the system role is a developer message; in the user role it joins a
    /// user message before it, or is its own after a tool output. A superseded state is
    /// omitted either way.
    #[test]
    fn states_take_the_dialects_role_and_never_join_tool_output() {
        use crate::provider::protocol::{StateLifetime::*, state};
        let text = |text: &str| json!({"type":"input_text","text":text});
        let mut req = request("gpt-5");
        req.context = vec![
            state("earlier", Superseded),
            tool_result("call_1", "search", json!(1), vec![], false).into(),
            state("now", Current),
        ];
        let as_user = Dialect::stateless();
        let input = encode(&req, Default::default(), &as_user).unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(
            input[1],
            json!({"type":"message","role":"user","content":[text("now")]})
        );
        let system = Dialect {
            state_role: StateRole::System,
            ..Dialect::stateless()
        };
        let input = encode(&req, Default::default(), &system).unwrap();
        assert_eq!(input[1]["role"], "developer");
        req.context = vec![user("hi").into(), state("now", Current)];
        let input = encode(&req, Default::default(), &as_user).unwrap();
        assert_eq!(
            json!(input),
            json!([{"type":"message","role":"user","content":[text("hi"), text("now")]}])
        );
    }

    fn explicit(role: StateRole) -> Dialect {
        Dialect {
            cache: Cache::Explicit,
            state_role: role,
            ..Dialect::stateless()
        }
    }

    fn answer(text: &str) -> Entry {
        Message::Assistant(vec![AssistantItem::text("answer", 0, text)]).into()
    }

    fn marked(value: &Value) -> bool {
        value.get("prompt_cache_breakpoint") == Some(&json!({"mode":"explicit"}))
    }

    fn has_checkpoint(value: &Value) -> bool {
        match value {
            Value::Object(object) => {
                object.contains_key("prompt_cache_breakpoint")
                    || object.values().any(has_checkpoint)
            }
            Value::Array(array) => array.iter().any(has_checkpoint),
            _ => false,
        }
    }

    #[test]
    fn automatic_cache_keeps_the_existing_wire_shape() {
        use crate::provider::protocol::{StateLifetime::*, state};
        let mut req = request("gpt-5");
        req.context = vec![
            user("stable").into(),
            state("old", Persisted),
            answer("checking"),
            tool_result(
                "call_1",
                "search",
                json!({"failure":"details"}),
                vec![],
                true,
            )
            .into(),
            state("now", Current),
        ];
        let input = encode(&req, Default::default(), &Dialect::stateless()).unwrap();
        assert!(!has_checkpoint(&json!(input)));
        assert_eq!(
            input[0]["content"],
            json!([
                {"type":"input_text", "text":"stable"},
                {"type":"input_text", "text":"old"}
            ])
        );
        assert_eq!(
            input[2]["output"],
            json!(r#"{"result":{"failure":"details"},"is_error":true}"#)
        );
        assert_eq!(input[3]["content"][0]["text"], "now");
    }

    #[test]
    fn dynamic_turns_retain_old_checkpoints_and_advance_before_current_state() {
        use crate::provider::protocol::{StateLifetime::*, state};
        for role in [StateRole::User, StateRole::System] {
            let dialect = explicit(role);
            let mut req = request("gpt-5");
            req.context = vec![user("first").into(), state("snapshot 1", Current)];
            let first = encode(&req, Default::default(), &dialect).unwrap();
            let stable = first[0]["content"][0].clone();
            assert!(marked(&stable));
            if role == StateRole::User {
                assert_eq!(first[0]["content"].as_array().unwrap().len(), 2);
                assert_eq!(first[0]["content"][1]["text"], "snapshot 1");
                assert!(!marked(&first[0]));
                assert!(!marked(&first[0]["content"][1]));
            } else {
                assert_eq!(first[1]["role"], "developer");
                assert!(!has_checkpoint(&first[1]));
            }

            req.context = vec![
                user("first").into(),
                state("snapshot 1", Superseded),
                answer("one"),
                user("second").into(),
                state("snapshot 2", Current),
            ];
            let second = encode(&req, Default::default(), &dialect).unwrap();
            assert_eq!(second[0]["content"][0], stable);
            assert!(!has_checkpoint(&second[1]));
            assert!(marked(&second[2]["content"][0]));
            assert!(!json!(second).to_string().contains("snapshot 1"));

            req.context = vec![
                user("first").into(),
                state("snapshot 1", Superseded),
                answer("one"),
                user("second").into(),
                state("snapshot 2", Superseded),
                answer("two"),
                user("third").into(),
                state("snapshot 3", Current),
            ];
            let third = encode(&req, Default::default(), &dialect).unwrap();
            assert_eq!(third[0], second[0]);
            assert_eq!(third[2]["content"][0], second[2]["content"][0]);
            assert!(marked(&third[4]["content"][0]));
            let current = if role == StateRole::User {
                &third[4]["content"][1]
            } else {
                &third[5]["content"][0]
            };
            assert_eq!(current["text"], "snapshot 3");
            assert!(!marked(current));
        }
    }

    #[test]
    fn persisted_and_stateless_histories_reuse_the_complete_prior_prefix() {
        use crate::provider::protocol::{StateLifetime::Persisted, state};
        for persisted in [false, true] {
            for role in [StateRole::User, StateRole::System] {
                let dialect = explicit(role);
                let mut req = request("gpt-5");
                req.context = vec![user("first").into()];
                if persisted {
                    req.context.push(state("stable state", Persisted));
                }
                let first = encode(&req, Default::default(), &dialect).unwrap();
                if persisted {
                    let content = first.last().unwrap()["content"].as_array().unwrap();
                    assert!(marked(content.last().unwrap()));
                }
                let prefix = first.as_slice();
                req.context.extend([answer("one"), user("next").into()]);
                let second = encode(&req, Default::default(), &dialect).unwrap();
                assert_eq!(&second[..prefix.len()], prefix);
                assert!(marked(&second.last().unwrap()["content"][0]));
                // Only the newest seams that fit the budget are marked, the
                // prefix's end among them.
                for prompt in ["three", "four", "five", "six"] {
                    req.context.extend([answer("reply"), user(prompt).into()]);
                }
                let input = encode(&req, Default::default(), &dialect).unwrap();
                let mut seams = Vec::new();
                for [turn, next] in input.array_windows() {
                    if next["role"] == "assistant" {
                        seams.push(has_checkpoint(turn));
                    } else {
                        assert!(!has_checkpoint(turn), "{turn}");
                    }
                }
                seams.push(has_checkpoint(input.last().unwrap()));
                assert_eq!(seams, [false, false, true, true, true, true]);
            }
        }
    }

    #[tokio::test]
    async fn explicit_tool_outputs_keep_arrays_ids_errors_and_image_sidecars_across_turns() {
        use crate::provider::protocol::{StateLifetime::*, state};
        let dialect = explicit(StateRole::User);
        let mut req = request("gpt-5");
        let Message::Tool(mut results) =
            tool_result("call_1", "search", json!({"failed":true}), vec![], true)
        else {
            unreachable!()
        };
        let Message::Tool(mut with_images) = tool_result(
            "call_1",
            "search",
            json!({"found":42}),
            vec![image()],
            false,
        ) else {
            unreachable!()
        };
        with_images[0].call_id = "call_2".into();
        results.extend(with_images);
        req.context = vec![
            user("search").into(),
            Message::Assistant(assemble(vec![call_item()])).into(),
            Message::Tool(results).into(),
            state("now", Current),
            answer("rest assistant"),
            tool_result("call_1", "search", json!("rest output"), vec![], false).into(),
        ];
        let images = loaded(&mut req).await;
        let input = encode(&req, images.prepared(), &dialect).unwrap();
        assert_eq!(input[1]["call_id"], input[2]["call_id"]);
        assert_eq!(input[3]["call_id"], "call_2");
        for output in [&input[2], &input[3], &input[6]] {
            assert!(output["output"].is_array());
            assert!(!has_checkpoint(output));
        }
        assert_eq!(
            serde_json::from_str::<Value>(input[2]["output"][0]["text"].as_str().unwrap()).unwrap(),
            json!({"result":{"failed":true}, "is_error":true})
        );
        assert_eq!(
            input[4]["content"][0]["text"],
            "Images from tool search (call_id: call_2):"
        );
        assert!(marked(&input[4]["content"][0]));
        assert_eq!(
            input[4]["content"][1],
            json!({"type":"input_image", "image_url":image_data()})
        );
        assert_eq!(input[4]["content"][2]["text"], "now");
        assert!(!marked(&input[4]["content"][2]));

        req.context[3] = state("now", Superseded);
        req.context.push(answer("done"));
        req.context.push(state("next", Current));
        let second = encode(&req, images.prepared(), &dialect).unwrap();
        assert_eq!(&second[..4], &input[..4]);
        assert_eq!(
            &second[4]["content"].as_array().unwrap()[..2],
            &input[4]["content"].as_array().unwrap()[..2]
        );
        assert_eq!(
            second[6]["output"][0]["text"],
            input[6]["output"][0]["text"]
        );
        assert!(marked(&second[6]["output"][0]));
        assert_eq!(second[6]["call_id"], input[6]["call_id"]);
    }

    #[tokio::test]
    async fn unsupported_trailing_items_fall_back_without_mutating_native_replay() {
        let native = reasoning_item();
        let mut req = request("gpt-5");
        req.context = entries(vec![
            user("stable"),
            Message::Assistant(assemble(vec![
                native.clone(),
                call_item(),
                message("text", "answer"),
            ])),
            Message::User(vec![
                UserContent::Text {
                    text: String::new(),
                },
                UserContent::Attachment {
                    attachment: AttachmentRef::Image(image()),
                },
            ]),
        ]);
        let images = loaded(&mut req).await;
        let automatic = encode(&req, images.prepared(), &Dialect::stateless()).unwrap();
        let explicit = encode(&req, images.prepared(), &explicit(StateRole::User)).unwrap();
        assert!(marked(&explicit[0]["content"][0]));
        assert_eq!(explicit[1], native);
        assert_eq!(&explicit[1..], &automatic[1..]);
    }

    #[tokio::test]
    async fn empty_nonmarkable_and_detached_inputs_have_no_checkpoints() {
        let dialect = explicit(StateRole::User);
        let mut req = request("gpt-5");
        let contexts = [
            vec![],
            entries(vec![
                user(""),
                Message::Assistant(assemble(vec![reasoning_item(), call_item()])),
            ]),
            entries(vec![Message::User(vec![UserContent::Attachment {
                attachment: AttachmentRef::Image(image()),
            }])]),
        ];
        for context in contexts {
            req.context = context;
            let images = loaded(&mut req).await;
            let input = encode(&req, images.prepared(), &dialect).unwrap();
            assert!(!has_checkpoint(&json!(input)));
        }
        req.history_lifetime = HistoryLifetime::Detached;
        req.context = vec![
            user("first").into(),
            answer("response"),
            tool_result("call_1", "search", json!("result"), vec![], false).into(),
            answer("done"),
            user("tail").into(),
        ];
        let input = encode(&req, Default::default(), &dialect).unwrap();
        assert!(!has_checkpoint(&json!(input)));
        assert!(input[2]["output"].is_array());
    }

    #[test]
    fn first_current_state_is_the_cutoff_even_when_filtered_and_rest_has_assistants() {
        use crate::provider::protocol::{StateLifetime::*, state};
        let mut req = request("gpt-5");
        req.context = vec![
            user("prefix").into(),
            state("filtered current", Current),
            user("rest user").into(),
            state("visible current", Current),
            answer("rest assistant"),
            tool_result("call_1", "search", json!("rest tool"), vec![], false).into(),
            state("tail current", Current),
        ];
        let input = encode(&req, Default::default(), &explicit(StateRole::User)).unwrap();
        assert!(marked(&input[0]["content"][0]));
        assert!(!input.iter().skip(1).any(has_checkpoint));
        assert_eq!(input[1]["content"][0]["text"], "rest user");
        assert_eq!(input[1]["content"][1]["text"], "visible current");
        assert!(!json!(input).to_string().contains("filtered current"));
        assert!(input[3]["output"].is_array());
        assert_eq!(input[4]["content"][0]["text"], "tail current");
    }
}

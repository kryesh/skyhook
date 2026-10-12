//! Encode Responses input history, replay, runtime state, and explicit cache checkpoints.
use super::{Cache, Dialect, ProviderError, native::ItemType, reasoning_parts};
use crate::{
    named_enum::named_enum,
    provider::{
        ProviderErrorKind::InvalidRequest,
        codec::{
            StateRole,
            common::{image_url, own_replay, tagged, tool_text, user_parts},
        },
        protocol::{
            AssistantItem, Entry, HistoryLifetime, Message, ModelRequest, Replay, ReplayFormat,
            State, sent,
        },
    },
};
use serde_json::{Value, json};

pub(super) fn encode(
    request: &ModelRequest,
    dialect: &Dialect,
) -> Result<Vec<Value>, ProviderError> {
    let mut input = Vec::new();
    let (prefix, rest) = sent(&request.context);
    let checkpoints =
        dialect.cache == Cache::Explicit && request.history_lifetime == HistoryLifetime::Extends;
    // Reconstruct earlier writes at response seams before advancing to the prefix tail.
    // Current state may coalesce into that tail's wrapper, but not its marked text block.
    for entry in prefix {
        if checkpoints && matches!(entry, Entry::Message(Message::Assistant(_))) {
            mark_last(&mut input);
        }
        push_entry(request, dialect, &mut input, entry)?;
    }
    if checkpoints {
        mark_last(&mut input);
    }
    for entry in rest {
        push_entry(request, dialect, &mut input, entry)?;
    }
    Ok(input)
}

fn push_entry(
    request: &ModelRequest,
    dialect: &Dialect,
    input: &mut Vec<Value>,
    entry: &Entry,
) -> Result<(), ProviderError> {
    let message = match entry {
        Entry::Message(message) => message,
        Entry::State(state) => {
            push_state(dialect.state_role, input, state);
            return Ok(());
        }
    };
    match message {
        Message::User(parts) => {
            let content = user_parts(request, parts, "input_text", |image| {
                Ok(json!({"type":"input_image", "image_url":image_url(request, image)?}))
            })?;
            input.push(json!({"type":"message", "role":"user", "content":content}));
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
                        ) {
                            if tagged(native) != Some(ItemType::Reasoning)
                                || reasoning_parts(native).is_err()
                            {
                                return Err(InvalidRequest.error(
                                    "Responses reasoning replay is not a readable reasoning item",
                                ));
                            }
                            input.push(native.clone());
                        }
                    }
                    // `type` is optional in the spec, but some servers
                    // cannot identify an assistant item without it.
                    AssistantItem::Text { blocks, .. } => {
                        for block in blocks {
                            input.push(json!({
                                "type":"message", "role":"assistant",
                                "content":[{"type":"output_text", "text":block.text}]
                            }));
                        }
                    }
                    AssistantItem::ToolCall { call, .. } => {
                        input.push(json!({"type":"function_call", "call_id":call.id(),
                                "name":call.name(), "arguments":serde_json::to_string(call.arguments()).expect("JSON object serialization cannot fail")}));
                    }
                }
            }
        }
        Message::Tool(results) => {
            for result in results {
                if result.call_id.is_empty() {
                    return Err(InvalidRequest.error("Responses tool output requires a call ID"));
                }
                let output = match dialect.cache {
                    Cache::Automatic => json!(tool_text(result)),
                    Cache::Explicit => {
                        json!([{"type":"input_text", "text":tool_text(result)}])
                    }
                };
                input.push(json!({"type":"function_call_output",
                        "call_id":result.call_id, "output":output}));
                if !result.images.is_empty() {
                    let mut content = vec![json!({"type":"input_text", "text":format!(
                            "Images from tool {} (call_id: {}):", result.name, result.call_id)})];
                    for image in &result.images {
                        content.push(
                            json!({"type":"input_image", "image_url":image_url(request, image)?}),
                        );
                    }
                    input.push(json!({"type":"message", "role":"user", "content":content}));
                }
            }
        }
    }
    Ok(())
}

/// A state the model reads, in the dialect's role: a developer message, or user content
/// joining the user message before it. Responses cannot hide a superseded state, so it is
/// omitted.
fn push_state(role: StateRole, input: &mut Vec<Value>, state: &State) {
    if !state.is_read() {
        return;
    }
    let text = || json!({"type":"input_text", "text":state.text});
    match role {
        StateRole::System => {
            input.push(json!({"type":"message", "role":"developer", "content":[text()]}));
        }
        StateRole::User => match input.last_mut() {
            Some(previous) if previous["type"] == "message" && previous["role"] == "user" => {
                previous["content"]
                    .as_array_mut()
                    .expect("constructed array")
                    .push(text());
            }
            _ => input.push(json!({"type":"message", "role":"user", "content":[text()]})),
        },
    }
}

named_enum! {
    #[derive(Clone, Copy, PartialEq, Eq)]
    parsed enum Carrier {
        Message = "message",
        ToolOutput = "function_call_output",
    }
}

named_enum! {
    #[derive(Clone, Copy, PartialEq, Eq)]
    parsed enum Part {
        InputText = "input_text",
    }
}

/// Mark the last supported, nonempty text block, leaving opaque native items alone.
/// Repeated response seams may select the same block; insertion is idempotent.
fn mark_last(input: &mut [Value]) {
    for item in input.iter_mut().rev() {
        let field = match tagged(item) {
            Some(Carrier::Message) => "content",
            Some(Carrier::ToolOutput) => "output",
            None => continue,
        };
        let Some(parts) = item.get_mut(field).and_then(Value::as_array_mut) else {
            continue;
        };
        for part in parts.iter_mut().rev() {
            if tagged(part) == Some(Part::InputText)
                && part
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.is_empty())
            {
                part.as_object_mut()
                    .expect("tagged object")
                    .insert("prompt_cache_breakpoint".into(), json!({"mode":"explicit"}));
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        Decoder,
        tests::{assemble_with, call_item, completed, reasoning_item},
    };
    use super::*;
    use crate::media::{AttachmentRef, ImageRef};
    use crate::provider::{
        codec::common::tests::{envelope, image, notes, request, scope},
        http::errors::ErrorSignals,
        protocol::{Binding, Outcome, ToolResult, UserContent, entries},
    };

    fn assemble(output: Vec<Value>) -> Vec<AssistantItem> {
        let decoder = Decoder::new(
            "gpt-5".into(),
            scope(),
            &Dialect::stateless(),
            ErrorSignals::NONE,
        );
        assemble_with(decoder, vec![completed(output)])
            .unwrap()
            .completion
            .items()
            .to_vec()
    }

    fn text_item(id: &str, text: &str) -> Value {
        json!({"type":"message", "id":id, "role":"assistant",
            "content":[{"type":"output_text", "text":text}]})
    }

    fn user(text: &str) -> Message {
        Message::User(vec![UserContent::Text { text: text.into() }])
    }

    fn tool_result(result: Value, images: Vec<ImageRef>, is_error: bool) -> Message {
        Message::Tool(vec![ToolResult {
            call_id: "call_1".into(),
            name: "search".into(),
            result,
            images,
            is_error,
        }])
    }

    #[test]
    fn user_attachments_and_tool_images_require_loaded_blobs() {
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
            tool_result(json!({"answer":42, "error":null}), vec![image()], true),
        ]);
        assert!(encode(&req, &Dialect::stateless()).is_err());
        // Load the fixture blobs as the session store would.
        req.blobs.insert(image().blob, b"\x01\x02\x03".to_vec());
        req.blobs.insert(notes().blob, b"notes".to_vec());
        let input = encode(&req, &Dialect::stateless()).unwrap();
        let image_url = "data:image/png;base64,AQID";
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
                encode(&req, &Dialect::stateless()).unwrap(),
                Vec::<Value>::new()
            );
        }
    }

    #[test]
    fn encrypted_reasoning_replays_only_to_its_model_beside_tools() {
        let mut req = request("gpt-5");
        // No display summary is required for native reasoning to be replayable.
        let native = json!({"type":"reasoning", "id":"rs_opaque", "summary":[],
            "encrypted_content":"opaque+/=", "future_state":{"signature":"unchanged"},
            "content":[{"type":"reasoning_text", "text":"native reasoning text"}]});
        let decoder = Decoder::new(
            req.model.to_string(),
            scope(),
            &Dialect::stateless(),
            ErrorSignals::NONE,
        );
        let reduced =
            assemble_with(decoder, vec![completed(vec![native.clone(), call_item()])]).unwrap();
        assert_eq!(reduced.completion.outcome(), Outcome::ToolUse);
        let items = reduced.items().to_vec();
        assert_eq!(
            items[0].reasoning_text().as_deref(),
            Some("native reasoning text")
        );
        req.context = entries(vec![
            Message::Assistant(items),
            tool_result(json!({"found":true}), vec![], false),
        ]);
        let input = encode(&req, &Dialect::stateless()).unwrap();
        assert_eq!(input[0], native);
        assert_eq!(
            (&input[1]["type"], &input[2]["type"]),
            (&json!("function_call"), &json!("function_call_output"))
        );
        assert_eq!(input[1]["call_id"], input[2]["call_id"]);
        // Another model's reasoning stays behind; the call and its output do not.
        req.model = "gpt-6".parse().unwrap();
        let wire = encode(&req, &Dialect::stateless()).unwrap();
        assert_eq!(wire, vec![input[1].clone(), input[2].clone()]);
    }

    #[test]
    fn terminal_only_output_replays_across_tool_and_followup_turns() {
        let mut native_reasoning = reasoning_item();
        native_reasoning["encrypted_content"] = json!("mock-replay-state");
        let items = assemble(vec![
            native_reasoning.clone(),
            text_item("text-terminal", "checking"),
            call_item(),
        ]);
        let mut req = request("gpt-5");
        req.context = entries(vec![
            user("look it up"),
            Message::Assistant(items),
            tool_result(json!({"value":42}), vec![], false),
        ]);
        let input = encode(&req, &Dialect::stateless()).unwrap();
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
            .push(Entry::from(Message::Assistant(assemble(vec![text_item(
                "answer", "42",
            )]))));
        req.context.push(Entry::from(user("thanks")));
        let replayed = encode(&req, &Dialect::stateless()).unwrap();
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
            tool_result(json!(1), vec![], false).into(),
            state("now", Current),
        ];
        let as_user = Dialect::stateless();
        let input = encode(&req, &as_user).unwrap();
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
        let input = encode(&req, &system).unwrap();
        assert_eq!(input[1]["role"], "developer");
        req.context = vec![user("hi").into(), state("now", Current)];
        let input = encode(&req, &as_user).unwrap();
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
            tool_result(json!({"failure":"details"}), vec![], true).into(),
            state("now", Current),
        ];
        let input = encode(&req, &Dialect::stateless()).unwrap();
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
            let first = encode(&req, &dialect).unwrap();
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
            let second = encode(&req, &dialect).unwrap();
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
            let third = encode(&req, &dialect).unwrap();
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
                let first = encode(&req, &dialect).unwrap();
                let prefix = first.as_slice();
                req.context.extend([answer("one"), user("next").into()]);
                let second = encode(&req, &dialect).unwrap();
                assert_eq!(&second[..prefix.len()], prefix);
                assert!(marked(&second.last().unwrap()["content"][0]));
            }
        }
    }

    #[test]
    fn explicit_tool_outputs_keep_arrays_ids_errors_and_image_sidecars_across_turns() {
        use crate::provider::protocol::{StateLifetime::*, state};
        let dialect = explicit(StateRole::User);
        let mut req = request("gpt-5");
        req.blobs.insert(image().blob, b"\x01\x02\x03".to_vec());
        let Message::Tool(mut results) = tool_result(json!({"failed":true}), vec![], true) else {
            unreachable!()
        };
        let Message::Tool(mut with_images) = tool_result(json!({"found":42}), vec![image()], false)
        else {
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
            tool_result(json!("rest output"), vec![], false).into(),
        ];
        let input = encode(&req, &dialect).unwrap();
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
            json!({"type":"input_image", "image_url":"data:image/png;base64,AQID"})
        );
        assert_eq!(input[4]["content"][2]["text"], "now");
        assert!(!marked(&input[4]["content"][2]));

        req.context[3] = state("now", Superseded);
        req.context.push(answer("done"));
        req.context.push(state("next", Current));
        let second = encode(&req, &dialect).unwrap();
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

    #[test]
    fn historical_seams_are_not_limited_to_the_servers_latest_writes() {
        let mut req = request("gpt-5");
        req.context.clear();
        let prompts = ["one", "two", "three", "four", "five", "six"];
        for prompt in prompts {
            req.context.extend([user(prompt).into(), answer("reply")]);
        }
        let input = encode(&req, &explicit(StateRole::User)).unwrap();
        assert_eq!(input.len(), prompts.len() * 2);
        for ([user, answer], prompt) in input.as_chunks::<2>().0.iter().zip(prompts) {
            assert_eq!(user["content"][0]["text"], prompt);
            assert!(marked(&user["content"][0]));
            assert!(!has_checkpoint(answer));
        }
    }

    #[test]
    fn unsupported_trailing_items_fall_back_without_mutating_native_replay() {
        let native = reasoning_item();
        let mut req = request("gpt-5");
        req.blobs.insert(image().blob, b"\x01\x02\x03".to_vec());
        req.context = entries(vec![
            user("stable"),
            Message::Assistant(assemble(vec![
                native.clone(),
                call_item(),
                text_item("text", "answer"),
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
        let automatic = encode(&req, &Dialect::stateless()).unwrap();
        let explicit = encode(&req, &explicit(StateRole::User)).unwrap();
        assert!(marked(&explicit[0]["content"][0]));
        assert_eq!(explicit[1], native);
        assert_eq!(&explicit[1..], &automatic[1..]);
    }

    #[test]
    fn empty_nonmarkable_and_detached_inputs_have_no_checkpoints() {
        let dialect = explicit(StateRole::User);
        let mut req = request("gpt-5");
        req.blobs.insert(image().blob, b"\x01\x02\x03".to_vec());
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
            let input = encode(&req, &dialect).unwrap();
            assert!(!has_checkpoint(&json!(input)));
        }
        req.history_lifetime = HistoryLifetime::Detached;
        req.context = vec![
            user("first").into(),
            answer("response"),
            tool_result(json!("result"), vec![], false).into(),
            answer("done"),
            user("tail").into(),
        ];
        let input = encode(&req, &dialect).unwrap();
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
            tool_result(json!("rest tool"), vec![], false).into(),
            state("tail current", Current),
        ];
        let input = encode(&req, &explicit(StateRole::User)).unwrap();
        assert!(marked(&input[0]["content"][0]));
        assert!(!input.iter().skip(1).any(has_checkpoint));
        assert_eq!(input[1]["content"][0]["text"], "rest user");
        assert_eq!(input[1]["content"][1]["text"], "visible current");
        assert!(!json!(input).to_string().contains("filtered current"));
        assert!(input[3]["output"].is_array());
        assert_eq!(input[4]["content"][0]["text"], "tail current");
    }

    #[test]
    fn only_supported_array_text_carriers_are_marked_idempotently() {
        let fallback = json!({"type":"message", "role":"user", "content":[
            {"type":"input_text", "text":"stable"},
            {"type":"input_text", "text":""},
            {"type":"input_image", "image_url":"image"},
            {"type":"input_file", "file_id":"file"}
        ]});
        let unsupported = vec![
            json!({"type":"reasoning", "content":[{"type":"input_text", "text":"opaque"}]}),
            json!({"type":"function_call", "arguments":"{}"}),
            json!({"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"answer"}]}),
            json!({"type":"function_call_output", "output":"string"}),
            json!({"type":"function_call_output", "output":[{"type":"input_text", "text":42}]}),
            json!({"type":"future", "content":[{"type":"input_text", "text":"unknown"}]}),
        ];
        let mut input = vec![fallback.clone()];
        input.extend(unsupported.clone());
        mark_last(&mut input);
        assert_eq!(&input[1..], unsupported);
        let mut expected = fallback;
        expected["content"][0]["prompt_cache_breakpoint"] = json!({"mode":"explicit"});
        assert_eq!(input[0], expected);
        let marked = input.clone();
        mark_last(&mut input);
        assert_eq!(input, marked);

        let mut output = vec![
            json!({"type":"function_call_output", "call_id":"call", "output":[
                {"type":"input_text", "text":"first"},
                {"type":"input_text", "text":"last"}
            ]}),
        ];
        mark_last(&mut output);
        assert_eq!(output[0]["call_id"], "call");
        assert!(
            output[0]["output"][0]
                .get("prompt_cache_breakpoint")
                .is_none()
        );
        assert_eq!(
            output[0]["output"][1]["prompt_cache_breakpoint"],
            json!({"mode":"explicit"})
        );
    }
}

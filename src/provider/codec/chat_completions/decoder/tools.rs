//! Assemble sequential tool-call fragments into calls with usable IDs.
use super::super::{NATIVE, wire};
use super::{Block, Decoder};
use crate::provider::{
    ProviderError,
    codec::common::{self, parse_tool_arguments},
    protocol::{ItemKind, ResponseEvent, ToolCall},
};
use serde_json::{Map, Value};
use std::collections::BTreeSet;

/// Hex digits of the digest that derived call IDs share.
const CALL_ID_ORIGIN_LEN: usize = 16;

/// The call IDs of one response. Server-supplied IDs are reserved so derived
/// ones never collide with them.
pub(super) struct CallIds<'a> {
    reserved: BTreeSet<&'a str>,
    issued: BTreeSet<String>,
    origin: Option<String>,
}

impl Decoder {
    /// Apply a delta's tool fragments, the legacy `function_call` among them.
    pub(super) fn tool_deltas(&mut self, delta: &wire::Delta, events: &mut Vec<ResponseEvent>) {
        let legacy = delta.legacy_call();
        // Unindexed entries seen in this delta; another named entry is a new call.
        let mut touched = BTreeSet::new();
        for call in delta.tool_calls.iter().flatten().chain(legacy.as_ref()) {
            let index = self.tool_index(call, &touched);
            if call.index.is_none() {
                touched.insert(index);
            }
            // One delta may carry a header and argument fragments for the
            // same index. Apply each entry in wire order, not as a map.
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
                    events.push(common::delta(id, ItemKind::ToolCall, fragment.clone()));
                }
            }
        }
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
                .next_back()
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

    pub(super) fn call_ids(&self) -> CallIds<'_> {
        let reserved = self.blocks.iter().filter_map(|block| match block {
            Block::Tool { call_id, .. } if !call_id.trim().is_empty() => Some(call_id.as_str()),
            _ => None,
        });
        CallIds {
            reserved: reserved.collect(),
            issued: BTreeSet::new(),
            origin: None,
        }
    }

    /// The call of the tool block at `index`, once its name and arguments make
    /// it usable. A missing or repeated ID is derived from the response.
    pub(super) fn tool_call(
        &self,
        index: usize,
        ids: &mut CallIds<'_>,
        call_id: &str,
        name: &str,
        arguments: &str,
    ) -> Result<ToolCall, ProviderError> {
        let arguments = tool_arguments(name, arguments)?;
        let mut call_id = call_id.to_owned();
        if call_id.trim().is_empty() || ids.issued.contains(&call_id) {
            let origin = ids.origin.get_or_insert_with(|| self.call_id_origin());
            call_id = std::iter::once(format!("call_{origin}_{index}"))
                .chain((1..).map(|n| format!("call_{origin}_{index}_{n}")))
                .find(|candidate| {
                    !ids.reserved.contains(candidate.as_str()) && !ids.issued.contains(candidate)
                })
                .expect("an unused ID exists");
        }
        ids.issued.insert(call_id.clone());
        ToolCall::new(call_id, name, Value::Object(arguments)).map_err(|error| NATIVE.error(error))
    }

    /// Deterministic origin for missing IDs: a digest of the request and the
    /// response ID (or content), so IDs differ across turns and stay hex.
    fn call_id_origin(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(self.request.to_bytes());
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
                        Block::Native { object, shape } => (b"r", [shape.text(object), "", ""]),
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
        digest.to_string()[..CALL_ID_ORIGIN_LEN].to_owned()
    }
}

/// A tool call's arguments, once its name and arguments make it usable.
pub(super) fn tool_arguments(
    name: &str,
    arguments: &str,
) -> Result<Map<String, Value>, ProviderError> {
    if name.trim().is_empty() {
        return Err(NATIVE.error("tool call has no function name"));
    }
    parse_tool_arguments(arguments).ok_or_else(|| NATIVE.error("invalid tool arguments JSON"))
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use crate::provider::http::transport::SseEvent;
    use crate::provider::protocol::{AssistantItem, ItemKind, Outcome, ResponseEvent, ToolCall};
    use serde_json::{Value, json};
    use std::collections::BTreeSet;

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
            let mut decoder = decoder();
            decoder.request = crate::media::BlobDigest::of(request.to_string().as_bytes());
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
            id.chars().all(crate::tool::registry::is_tool_name_char),
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

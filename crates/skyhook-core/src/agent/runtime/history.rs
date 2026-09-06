//! Read-only recovery of the caller's original conversation after compaction.

use std::sync::{Arc, OnceLock, Weak};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    identity::AgentId,
    provider::protocol::{AssistantContent, Message, UserContent},
    session::{EventRecord, SessionEvent},
    tool::{RegistryError, ToolError, ToolOptions, ToolRegistryBuilder},
};

use super::SessionRuntime;

const MAX_OUTPUT_BYTES: usize = 8192;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct HistoryArgs {
    /// Exact source reference, such as m42/b0, m43/b0/result, or c50/b0. Omit to browse all your history.
    source: Option<String>,
    /// Literal case-insensitive text filter. Keep the same filter while paging.
    query: Option<String>,
    /// Byte cursor returned by the previous page; with source, this is an offset within that source.
    cursor: Option<usize>,
    /// Last journal sequence in the snapshot. Reuse the returned through value when paging.
    through: Option<u64>,
    /// Maximum text chunks, from 1 to 100; defaults to 20. Responses are also limited to 8 KiB.
    limit: Option<usize>,
}

#[derive(Serialize, JsonSchema)]
struct HistoryOutput {
    entries: Vec<HistoryEntry>,
    next_cursor: Option<usize>,
    through: u64,
}

#[derive(Serialize, JsonSchema)]
struct HistoryEntry {
    source: String,
    /// Byte offset within this original source.
    offset: usize,
    text: String,
}

pub(super) fn register(
    builder: &mut ToolRegistryBuilder,
    runtime_slot: Arc<OnceLock<Weak<SessionRuntime>>>,
) -> Result<(), RegistryError> {
    builder.register::<HistoryArgs, HistoryOutput, _, _>(
        "history",
        "Read or search your original conversation and compaction messages. Source references match preserved evidence; use next_cursor and through with the same source/query to continue. Does not read other agents or consume job notifications.",
        ToolOptions::default(),
        move |context, args| {
            let runtime = runtime_slot.get().and_then(Weak::upgrade);
            async move {
                let runtime = runtime.ok_or_else(|| ToolError::Failed("session runtime is unavailable".to_owned()))?;
                let mut records = runtime.store.records().await;
                let through = args.through.unwrap_or_else(|| records.last().map_or(0, |record| record.sequence));
                records.retain(|record| record.sequence <= through);
                let mut result = page(&corpus(&records, &context.agent), &args)?;
                result.through = through;
                Ok(result)
            }
        },
    )?;
    Ok(())
}

fn corpus(records: &[EventRecord], agent: &AgentId) -> Vec<(String, String)> {
    let mut texts = Vec::new();
    for record in records.iter().filter(|record| &record.agent == agent) {
        let (prefix, message) = match &record.event {
            SessionEvent::MessageCommitted { message } => {
                (format!("m{}", record.sequence), message)
            }
            SessionEvent::Compaction { checkpoint } => {
                (format!("c{}", record.sequence), &checkpoint.message)
            }
            _ => continue,
        };
        match message {
            Message::User(blocks) => {
                for (index, block) in blocks.iter().enumerate() {
                    match block {
                        UserContent::Text { text }
                        | UserContent::ParentInput { text }
                        | UserContent::Compaction { text } => {
                            texts.push((format!("{prefix}/b{index}"), text.clone()))
                        }
                        UserContent::Runtime { text } if !text.starts_with("<skyhook_state>") => {
                            texts.push((format!("{prefix}/b{index}"), text.clone()))
                        }
                        UserContent::Runtime { .. } | UserContent::Image { .. } => {}
                    }
                }
            }
            Message::Assistant(blocks) => {
                for (index, block) in blocks.iter().enumerate() {
                    match block {
                        AssistantContent::Text { text } => {
                            texts.push((format!("{prefix}/b{index}"), text.clone()))
                        }
                        AssistantContent::ToolCall(call) => texts.push((
                            format!("{prefix}/b{index}/arguments"),
                            serde_json::to_string_pretty(&call.arguments)
                                .expect("JSON value serializes"),
                        )),
                        AssistantContent::Reasoning { .. } => {}
                    }
                }
            }
            Message::Tool(results) => {
                for (index, result) in results.iter().enumerate() {
                    texts.push((
                        format!("{prefix}/b{index}/result"),
                        serde_json::to_string_pretty(&result.result)
                            .expect("JSON value serializes"),
                    ));
                    texts.push((
                        format!("{prefix}/b{index}/console"),
                        result.console_output.clone(),
                    ));
                }
            }
        }
    }
    texts
}

fn page(corpus: &[(String, String)], args: &HistoryArgs) -> Result<HistoryOutput, ToolError> {
    let limit = args.limit.unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return Err(ToolError::InvalidArguments(
            "limit must be between 1 and 100".to_owned(),
        ));
    }
    if args
        .source
        .as_ref()
        .is_some_and(|source| !corpus.iter().any(|(id, _)| id == source))
    {
        return Err(ToolError::InvalidArguments(
            "source was not found in your conversation".to_owned(),
        ));
    }
    let query = args.query.as_ref().map(|query| query.to_lowercase());
    let matches: Vec<_> = corpus
        .iter()
        .filter(|(source, text)| {
            args.source.as_ref().is_none_or(|id| id == source)
                && query
                    .as_ref()
                    .is_none_or(|query| text.to_lowercase().contains(query))
        })
        .collect();
    let total = matches.iter().map(|(_, text)| text.len()).sum::<usize>();
    let mut cursor = args.cursor.unwrap_or(0);
    if cursor > total {
        return Err(ToolError::InvalidArguments(
            "cursor is past the matching history".to_owned(),
        ));
    }
    let mut output = HistoryOutput {
        entries: Vec::new(),
        next_cursor: None,
        through: u64::MAX,
    };
    let mut base = 0;
    for (source, text) in matches {
        if cursor >= base + text.len() {
            base += text.len();
            continue;
        }
        let mut offset = cursor - base;
        if !text.is_char_boundary(offset) {
            return Err(ToolError::InvalidArguments(
                "cursor must be a UTF-8 character boundary".to_owned(),
            ));
        }
        while offset < text.len() {
            if output.entries.len() == limit {
                output.next_cursor = Some(cursor);
                return Ok(output);
            }
            // Six bytes covers JSON escaping for each input byte. Reserve metadata and
            // the largest cursor before choosing a chunk, so the entire JSON stays bounded.
            output.next_cursor = Some(usize::MAX);
            let used = serde_json::to_vec(&output)?.len();
            let available = MAX_OUTPUT_BYTES.saturating_sub(used + source.len() + 100) / 6;
            if available < 4 {
                output.next_cursor = Some(cursor);
                return Ok(output);
            }
            let mut end = (offset + available).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            output.entries.push(HistoryEntry {
                source: source.clone(),
                offset,
                text: text[offset..end].to_owned(),
            });
            cursor += end - offset;
            offset = end;
        }
        base += text.len();
    }
    output.next_cursor = None;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::SessionId;

    fn args() -> HistoryArgs {
        HistoryArgs {
            source: None,
            query: None,
            cursor: None,
            through: None,
            limit: None,
        }
    }

    #[test]
    fn pagination_recovers_huge_unicode_and_escaped_text_without_loss() {
        let original = "😀\\\"\n\u{0001} state ".repeat(4000);
        let corpus = vec![("m2/b0".to_owned(), original.clone())];
        let mut input = args();
        let mut recovered = String::new();
        loop {
            let result = page(&corpus, &input).unwrap();
            assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_OUTPUT_BYTES);
            for entry in &result.entries {
                assert_eq!(entry.offset, recovered.len());
                recovered.push_str(&entry.text);
            }
            let Some(cursor) = result.next_cursor else {
                break;
            };
            assert!(cursor > input.cursor.unwrap_or(0));
            input.cursor = Some(cursor);
        }
        assert_eq!(recovered, original);
    }

    #[test]
    fn search_source_and_cursor_validation() {
        let corpus = vec![
            ("m2/b0".to_owned(), "Chosen APPROACH".to_owned()),
            ("m3/b0".to_owned(), "😀 discarded".to_owned()),
        ];
        let mut input = args();
        input.query = Some("approach".to_owned());
        let result = page(&corpus, &input).unwrap();
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].source, "m2/b0");
        input.query = None;
        input.source = Some("m3/b0".to_owned());
        input.cursor = Some(1);
        assert!(page(&corpus, &input).is_err());
        input.cursor = Some(4);
        assert_eq!(page(&corpus, &input).unwrap().entries[0].text, " discarded");
        input.limit = Some(101);
        assert!(page(&corpus, &input).is_err());
        input.limit = None;
        input.source = Some("m100/b0".to_owned());
        assert!(page(&corpus, &input).is_err());
    }

    #[test]
    fn corpus_is_limited_to_own_agent_and_excludes_reasoning() {
        let owner = AgentId::root(SessionId::from_bytes([1; 16]));
        let record = |sequence, agent, message| EventRecord {
            version: crate::session::SESSION_FORMAT_VERSION,
            sequence,
            timestamp_millis: 0,
            agent,
            event: SessionEvent::MessageCommitted { message },
        };
        let records = vec![
            record(
                1,
                owner.clone(),
                Message::Assistant(vec![
                    AssistantContent::Reasoning {
                        text: "private reasoning".to_owned(),
                        opaque: None,
                    },
                    AssistantContent::Text {
                        text: "original plan".to_owned(),
                    },
                ]),
            ),
            record(
                2,
                owner.child(1),
                Message::User(vec![UserContent::Text {
                    text: "child task".to_owned(),
                }]),
            ),
            record(
                3,
                owner.clone(),
                Message::User(vec![
                    UserContent::Runtime {
                        text: "Job 7 completed: decisive output".to_owned(),
                    },
                    UserContent::Runtime {
                        text: "<skyhook_state>{}</skyhook_state>".to_owned(),
                    },
                ]),
            ),
        ];
        assert_eq!(
            corpus(&records, &owner),
            vec![
                ("m1/b1".to_owned(), "original plan".to_owned()),
                (
                    "m3/b0".to_owned(),
                    "Job 7 completed: decisive output".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn corpus_uses_compaction_evidence_references_and_recovers_checkpoints() {
        let owner = AgentId::root(SessionId::from_bytes([1; 16]));
        let result = serde_json::json!({"important": "evidence"});
        let records = vec![
            EventRecord {
                version: crate::session::SESSION_FORMAT_VERSION,
                sequence: 2,
                timestamp_millis: 0,
                agent: owner.clone(),
                event: SessionEvent::MessageCommitted {
                    message: Message::Tool(vec![crate::provider::protocol::ToolResult {
                        call_id: "call".to_owned(),
                        name: "script".to_owned(),
                        result: result.clone(),
                        console_output: "exact console\n".to_owned(),
                        images: Vec::new(),
                        is_error: false,
                    }]),
                },
            },
            EventRecord {
                version: crate::session::SESSION_FORMAT_VERSION,
                sequence: 5,
                timestamp_millis: 0,
                agent: owner.clone(),
                event: SessionEvent::Compaction {
                    checkpoint: crate::session::CompactionCheckpoint {
                        schema_version: 1,
                        todos: Vec::new(),
                        previous: None,
                        frontier: 2,
                        message: Message::User(vec![UserContent::Compaction {
                            text: "checkpoint".to_owned(),
                        }]),
                        retained: Vec::new(),
                        request: 4,
                        max_context: 128000,
                        before_tokens: 100000,
                        after_tokens: 10000,
                    },
                },
            },
        ];
        assert_eq!(
            corpus(&records, &owner),
            vec![
                (
                    "m2/b0/result".to_owned(),
                    serde_json::to_string_pretty(&result).unwrap()
                ),
                ("m2/b0/console".to_owned(), "exact console\n".to_owned()),
                ("c5/b0".to_owned(), "checkpoint".to_owned()),
            ]
        );
    }
}

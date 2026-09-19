//! Reassemble session events from normalized rows.

use std::collections::HashMap;

use libsql::Row;
use serde::de::DeserializeOwned;

use super::{Db, DbResult, corrupt};
use crate::{
    agent::TodoItem,
    execution::ExecutionLocation,
    identity::{AgentId, EventId, JobId, SessionId},
    media::{AttachmentRef, BlobDigest, BlobRef, ImageRef, TextRef},
    provider::{
        profile::ModelProfile,
        protocol::{
            AssistantBlock, AssistantItem, BlockContent, Message, ReplayEnvelope, ResponseSchema,
            StopReason, SystemSegment, ToolCall, ToolDefinition, ToolResult, Usage, UserContent,
        },
    },
    session::{
        CompactionCheckpoint, EventRecord, ModelCallOrigin, ModelContext, ProfileSnapshot,
        SessionEvent,
    },
    target::{SshOptions, TargetAuth, TargetDefinition, TargetType},
    tool::{Denial, policy::Capability},
};

pub(in crate::session) fn parse_variant<T: DeserializeOwned>(text: String) -> DbResult<T> {
    serde_json::from_value(serde_json::Value::String(text))
        .map_err(|error| corrupt(error.to_string()))
}

fn parse_json<T: DeserializeOwned>(text: &str) -> DbResult<T> {
    serde_json::from_str(text).map_err(|error| corrupt(error.to_string()))
}

#[cfg(unix)]
pub(in crate::session) fn path(bytes: Vec<u8>) -> std::path::PathBuf {
    std::path::PathBuf::from(
        <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(bytes),
    )
}

#[cfg(not(unix))]
pub(in crate::session) fn path(bytes: Vec<u8>) -> std::path::PathBuf {
    std::path::PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}

fn bytes<const N: usize>(value: Vec<u8>) -> DbResult<[u8; N]> {
    value
        .try_into()
        .map_err(|_| corrupt("identifier has the wrong length"))
}

pub(in crate::session) fn u64_of(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

fn job(value: i64) -> DbResult<JobId> {
    JobId::new(u64_of(value)).map_err(|error| corrupt(error.to_string()))
}

/// Index rows of one query by their first column.
fn keyed<T>(
    db: &Db,
    sql: &str,
    mut map: impl FnMut(&Row) -> DbResult<T>,
) -> DbResult<HashMap<i64, T>> {
    Ok(db
        .query(sql, Vec::new(), |row| Ok((row.get::<i64>(0)?, map(row)?)))?
        .into_iter()
        .collect())
}

/// Group rows of one query (ordered by position) under their first column.
fn grouped<T>(
    db: &Db,
    sql: &str,
    mut map: impl FnMut(&Row) -> DbResult<T>,
) -> DbResult<HashMap<i64, Vec<T>>> {
    let mut groups: HashMap<i64, Vec<T>> = HashMap::new();
    for (key, value) in db.query(sql, Vec::new(), |row| Ok((row.get::<i64>(0)?, map(row)?)))? {
        groups.entry(key).or_default().push(value);
    }
    Ok(groups)
}

/// kind, text, blob (digest, length), image format, file name
type UserPart = (
    String,
    Option<String>,
    Option<(Vec<u8>, u64)>,
    Option<String>,
    Option<String>,
);
/// position, provider id, text, tool call (id, name, arguments)
type Block = (
    i64,
    String,
    Option<String>,
    Option<(String, String, String)>,
);

struct Messages {
    roles: HashMap<i64, String>,
    parts: HashMap<i64, Vec<UserPart>>,
    items: HashMap<i64, Vec<(i64, i64, String, String)>>,
    replays: HashMap<i64, ReplayEnvelope>,
    blocks: HashMap<i64, Vec<Block>>,
    /// call id, name, result, is_error
    results: HashMap<i64, (String, String, String, bool)>,
    images: HashMap<i64, Vec<ImageRef>>,
}

fn blob(sha256: Vec<u8>, bytes: u64) -> DbResult<BlobRef> {
    Ok(BlobRef {
        sha256: BlobDigest::from_bytes(self::bytes(sha256)?),
        bytes,
    })
}

/// An image slot selected as (blob, length(blob bytes), format, file) from column `at`.
fn image(row: &Row, at: i32) -> DbResult<ImageRef> {
    Ok(ImageRef {
        blob: blob(row.get(at)?, row.get(at + 1)?)?,
        format: parse_variant(row.get(at + 2)?)?,
        file: row.get(at + 3)?,
    })
}

fn call(row: &Row, at: i32) -> DbResult<Option<(String, String, String)>> {
    Ok(match row.get::<Option<String>>(at)? {
        Some(id) => Some((id, row.get(at + 1)?, row.get(at + 2)?)),
        None => None,
    })
}

impl Messages {
    fn load(db: &Db) -> DbResult<Self> {
        Ok(Self {
            roles: keyed(db, "SELECT id, role FROM message", |row| Ok(row.get(1)?))?,
            parts: grouped(
                db,
                "SELECT p.message, p.kind, p.text, p.blob, length(b.bytes), p.image_format, p.file \
                 FROM user_part p LEFT JOIN blob b ON b.sha256 = p.blob \
                 ORDER BY p.message, p.position",
                |row| {
                    let blob = match row.get::<Option<Vec<u8>>>(3)? {
                        Some(digest) => Some((digest, row.get(4)?)),
                        None => None,
                    };
                    Ok((row.get(1)?, row.get(2)?, blob, row.get(5)?, row.get(6)?))
                },
            )?,
            items: grouped(
                db,
                "SELECT message, id, position, provider_id, kind FROM assistant_item \
                 ORDER BY message, position",
                |row| Ok((row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )?,
            replays: keyed(
                db,
                "SELECT item, version, protocol, model, scope, payload, conversation_bound \
                 FROM reasoning_replay",
                |row| {
                    Ok(ReplayEnvelope {
                        version: row.get(1)?,
                        protocol: row.get(2)?,
                        model: row.get(3)?,
                        scope: row.get(4)?,
                        payload: parse_json(&row.get::<String>(5)?)?,
                        conversation_bound: row.get(6)?,
                    })
                },
            )?,
            blocks: grouped(
                db,
                "SELECT b.item, b.position, b.provider_id, b.text, c.call_id, c.name, c.arguments \
                 FROM assistant_block b LEFT JOIN tool_call c ON c.block = b.id \
                 ORDER BY b.item, b.position",
                |row| Ok((row.get(1)?, row.get(2)?, row.get(3)?, call(row, 4)?)),
            )?,
            results: keyed(
                db,
                "SELECT r.message, c.call_id, c.name, r.result, r.is_error FROM tool_result r \
                 JOIN tool_call c ON c.block = r.call",
                |row| Ok((row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )?,
            images: grouped(
                db,
                "SELECT i.result, i.blob, length(b.bytes), i.format, i.file \
                 FROM tool_result_image i JOIN blob b ON b.sha256 = i.blob \
                 ORDER BY i.result, i.position",
                |row| image(row, 1),
            )?,
        })
    }

    fn message(&self, id: i64) -> DbResult<Message> {
        let role = self
            .roles
            .get(&id)
            .ok_or_else(|| corrupt("referenced message is missing"))?;
        Ok(match role.as_str() {
            "user" => {
                let mut content = Vec::new();
                for (kind, text, blob, format, file) in self.parts.get(&id).into_iter().flatten() {
                    let text = || text.clone().ok_or_else(|| corrupt("text part has no text"));
                    content.push(match kind.as_str() {
                        "text" => UserContent::Text { text: text()? },
                        "runtime" => UserContent::Runtime { text: text()? },
                        "parent_input" => UserContent::ParentInput { text: text()? },
                        "compaction" => UserContent::Compaction { text: text()? },
                        "attachment" => {
                            let (digest, bytes) = blob
                                .clone()
                                .ok_or_else(|| corrupt("attachment has no blob"))?;
                            let (blob, file) = (self::blob(digest, bytes)?, file.clone());
                            let attachment = match format {
                                Some(format) => AttachmentRef::Image(ImageRef {
                                    file,
                                    format: parse_variant(format.clone())?,
                                    blob,
                                }),
                                None => AttachmentRef::Text(TextRef { file, blob }),
                            };
                            UserContent::Attachment { attachment }
                        }
                        other => return Err(corrupt(format!("unknown user part {other}"))),
                    });
                }
                Message::User(content)
            }
            "assistant" => {
                let mut items = Vec::new();
                for (item, position, provider_id, kind) in self.items.get(&id).into_iter().flatten()
                {
                    let mut blocks = Vec::new();
                    for (block_position, block_id, text, call) in
                        self.blocks.get(item).into_iter().flatten()
                    {
                        let content = match (kind.as_str(), text, call) {
                            ("text", Some(text), None) => BlockContent::Text { text: text.clone() },
                            ("reasoning", Some(text), None) => {
                                BlockContent::Reasoning { text: text.clone() }
                            }
                            ("tool_call", None, Some((call_id, name, arguments))) => {
                                BlockContent::ToolCall(
                                    ToolCall::new(
                                        call_id.clone(),
                                        name.clone(),
                                        parse_json(arguments)?,
                                    )
                                    .map_err(|error| corrupt(error.to_string()))?,
                                )
                            }
                            _ => return Err(corrupt("assistant block does not match its item")),
                        };
                        blocks.push(AssistantBlock {
                            id: block_id.clone(),
                            position: usize::try_from(*block_position).unwrap_or_default(),
                            content,
                        });
                    }
                    items.push(AssistantItem {
                        id: provider_id.clone(),
                        position: usize::try_from(*position).unwrap_or_default(),
                        kind: parse_variant(kind.clone())?,
                        blocks,
                        replay: self.replays.get(item).cloned(),
                    });
                }
                Message::Assistant(items)
            }
            "tool" => {
                let (call_id, name, result, is_error) = self
                    .results
                    .get(&id)
                    .ok_or_else(|| corrupt("tool message has no result"))?;
                Message::Tool(vec![ToolResult {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    result: parse_json(result)?,
                    images: self.images.get(&id).cloned().unwrap_or_default(),
                    is_error: *is_error,
                }])
            }
            other => return Err(corrupt(format!("unknown message role {other}"))),
        })
    }
}

fn capability(text: String) -> DbResult<Capability> {
    text.parse()
        .map_err(|error: crate::tool::policy::ParseCapabilityError| corrupt(error.to_string()))
}

/// Capability sets iterate in declaration order.
fn sorted(mut capabilities: Vec<Capability>) -> Vec<Capability> {
    capabilities.sort();
    capabilities
}

fn profiles(db: &Db) -> DbResult<HashMap<i64, ProfileSnapshot>> {
    keyed(
        db,
        "SELECT id, name, provider, model, reasoning, max_context, max_output, supports_images, \
         state_mode FROM model_profile",
        |row| {
            Ok(ProfileSnapshot {
                name: row.get(1)?,
                profile: ModelProfile {
                    provider: row.get(2)?,
                    model: row.get(3)?,
                    reasoning: row.get(4)?,
                    max_context: row.get(5)?,
                    max_output: row.get(6)?,
                    supports_images: row.get(7)?,
                    state_mode: parse_variant(row.get(8)?)?,
                },
            })
        },
    )
}

fn targets(db: &Db) -> DbResult<HashMap<i64, Vec<TargetDefinition>>> {
    let options = grouped(
        db,
        "SELECT revision, key, value FROM target_ssh_option ORDER BY revision, key",
        |row| Ok((row.get::<String>(1)?, row.get::<String>(2)?)),
    )?;
    let mut targets: HashMap<i64, Vec<TargetDefinition>> = HashMap::new();
    for (entry, target) in db.query(
        "SELECT r.entry, r.id, t.name, r.revision, r.source, r.host, r.workspace, r.ssh_user, \
         r.ssh_port, r.ssh_auth, r.ssh_key_path, r.ssh_external_agent, v.name, o.name \
         FROM target_revision r JOIN target t ON t.id = r.target \
         LEFT JOIN target v ON v.id = r.via LEFT JOIN target o ON o.id = r.origin \
         ORDER BY r.entry, r.id",
        Vec::new(),
        |row| {
            let auth = match (
                row.get::<String>(9)?.as_str(),
                row.get::<Option<Vec<u8>>>(10)?,
            ) {
                ("default", None) => TargetAuth::Default,
                ("agent", None) => TargetAuth::Agent,
                ("key", Some(key)) => TargetAuth::Key { path: path(key) },
                _ => return Err(corrupt("target authentication is inconsistent")),
            };
            let port = row.get::<Option<u32>>(8)?;
            Ok((
                row.get::<i64>(0)?,
                TargetDefinition {
                    name: row.get(2)?,
                    r#type: TargetType::Ssh,
                    host: row.get(5)?,
                    ssh: SshOptions {
                        user: row.get(7)?,
                        port: port
                            .and_then(|port| u16::try_from(port).ok())
                            .and_then(std::num::NonZeroU16::new),
                        auth,
                        external_agent: row.get(11)?,
                        options: options
                            .get(&row.get::<i64>(1)?)
                            .into_iter()
                            .flatten()
                            .cloned()
                            .collect(),
                    },
                    workspace: path(row.get(6)?),
                    via: row.get(12)?,
                    origin: row.get(13)?,
                    source: parse_variant(row.get(4)?)?,
                    revision: row.get(3)?,
                },
            ))
        },
    )? {
        targets.entry(entry).or_default().push(target);
    }
    Ok(targets)
}

fn event_id(value: Vec<u8>) -> DbResult<EventId> {
    Ok(EventId::from_bytes(bytes(value)?))
}

/// Decode every committed record in sequence order.
pub(in crate::session) fn decode_records(
    db: &Db,
    session: SessionId,
) -> DbResult<Vec<EventRecord>> {
    let messages = Messages::load(db)?;
    let profiles = profiles(db)?;
    let profile = |id: i64| {
        profiles
            .get(&id)
            .cloned()
            .ok_or_else(|| corrupt("model profile is missing"))
    };
    let targets = targets(db)?;
    let todos = grouped(
        db,
        "SELECT entry, text, status FROM todo_item ORDER BY entry, position",
        |row| {
            Ok(TodoItem {
                text: row.get(1)?,
                status: parse_variant(row.get(2)?)?,
            })
        },
    )?;
    let location = |target: String, workspace: Vec<u8>| ExecutionLocation {
        target,
        workspace: path(workspace),
    };
    // Parents always precede children, so one ordered pass builds every path.
    let mut agents: HashMap<i64, AgentId> = HashMap::new();
    for (id, parent, child_index) in db.query(
        "SELECT id, parent, child_index FROM agent ORDER BY id",
        Vec::new(),
        |row| {
            Ok((
                row.get::<i64>(0)?,
                row.get::<Option<i64>>(1)?,
                row.get::<Option<u32>>(2)?,
            ))
        },
    )? {
        let agent = match (parent, child_index) {
            (None, None) => AgentId::root(session),
            (Some(parent), Some(index)) => agents
                .get(&parent)
                .ok_or_else(|| corrupt("agent parent is missing"))?
                .child(index),
            _ => return Err(corrupt("agent path is inconsistent")),
        };
        agents.insert(id, agent);
    }
    let agent_of = |id: i64| -> DbResult<AgentId> {
        agents
            .get(&id)
            .cloned()
            .ok_or_else(|| corrupt("entry agent is missing"))
    };
    let agent_capabilities = grouped(
        db,
        "SELECT entry, capability FROM agent_capability",
        |row| capability(row.get(1)?),
    )?;
    let prompts = grouped(
        db,
        "SELECT prompt, text, cache FROM system_segment ORDER BY prompt, position",
        |row| {
            Ok(SystemSegment {
                text: row.get(1)?,
                cache: row.get(2)?,
            })
        },
    )?;
    let tools = grouped(
        db,
        "SELECT t.context, d.name, d.description, d.input_schema FROM model_context_tool t \
         JOIN tool_definition d ON d.id = t.tool ORDER BY t.context, t.position",
        |row| {
            Ok(ToolDefinition {
                name: row.get(1)?,
                description: row.get(2)?,
                input_schema: parse_json(&row.get::<String>(3)?)?,
            })
        },
    )?;
    let commits = grouped(
        db,
        "SELECT e.agent, m.entry FROM message_commit m JOIN entry e ON e.seq = m.entry \
         ORDER BY m.entry",
        |row| Ok(row.get::<u64>(1)?),
    )?;
    let omitted = grouped(
        db,
        "SELECT request, source FROM model_request_omitted",
        |row| Ok(row.get::<u64>(1)?),
    )?;
    let tails = grouped(
        db,
        "SELECT request, message FROM model_request_tail ORDER BY request, position",
        |row| messages.message(row.get(1)?),
    )?;
    let retained = grouped(
        db,
        "SELECT compaction, source FROM compaction_retained ORDER BY compaction, source",
        |row| Ok(row.get::<u64>(1)?),
    )?;
    let finish_images = grouped(
        db,
        "SELECT f.finish, f.blob, length(b.bytes), f.format, f.file FROM job_finish_image f \
         JOIN blob b ON b.sha256 = f.blob ORDER BY f.finish, f.position",
        |row| image(row, 1),
    )?;

    // Every other entry's event, built by its subtype's loader.
    let mut events: HashMap<i64, SessionEvent> = HashMap::new();
    macro_rules! load {
        ($sql:expr, |$row:ident| $event:expr) => {
            events.extend(keyed(db, $sql, |$row| Ok($event))?)
        };
    }
    load!("SELECT entry, kind, text FROM entry_text", |row| {
        let text = row.get(2)?;
        match row.get::<String>(1)?.as_str() {
            "title_set" => SessionEvent::TitleSet { title: text },
            "status" => SessionEvent::Status { message: text },
            _ => SessionEvent::AgentFailed { error: text },
        }
    });
    load!(
        "SELECT s.entry, a.id, a.parent, a.owner_job, a.available_depth, t.name, \
         s.location_workspace, s.profile FROM agent_start s \
         JOIN entry e ON e.seq = s.entry JOIN agent a ON a.id = e.agent \
         JOIN target t ON t.id = s.location_target",
        |row| SessionEvent::AgentStarted {
            parent: row.get::<Option<i64>>(2)?.map(agent_of).transpose()?,
            owner_job: row.get::<Option<i64>>(3)?.map(job).transpose()?,
            profile: row.get::<Option<i64>>(7)?.map(profile).transpose()?,
            available_depth: row.get(4)?,
            capabilities: sorted(
                agent_capabilities
                    .get(&row.get::<i64>(0)?)
                    .cloned()
                    .unwrap_or_default(),
            ),
            location: location(row.get(5)?, row.get(6)?),
        }
    );
    load!("SELECT entry, profile FROM model_selection", |row| {
        SessionEvent::ModelChanged {
            profile: profile(row.get(1)?)?,
        }
    });
    load!("SELECT entry, message FROM message_commit", |row| {
        SessionEvent::MessageCommitted {
            message: messages.message(row.get(1)?)?,
        }
    });
    load!(
        "SELECT entry, purpose, profile, system_prompt, response_schema_name, response_schema \
         FROM model_context",
        |row| {
            let seq = row.get::<i64>(0)?;
            let response_schema = match (row.get(4)?, row.get::<Option<String>>(5)?) {
                (Some(name), Some(schema)) => Some(ResponseSchema {
                    name,
                    schema: parse_json(&schema)?,
                }),
                _ => None,
            };
            SessionEvent::ModelContext {
                context: ModelContext {
                    purpose: parse_variant(row.get(1)?)?,
                    profile: profile(row.get(2)?)?,
                    system: prompts
                        .get(&row.get::<i64>(3)?)
                        .cloned()
                        .unwrap_or_default(),
                    tools: tools.get(&seq).cloned().unwrap_or_default(),
                    response_schema,
                },
            }
        }
    );
    load!(
        "SELECT r.entry, r.context, r.purpose, r.checkpoint, r.history_lifetime, \
         r.history_through, e.agent, coalesce(c.frontier, 0) FROM model_request r \
         JOIN entry e ON e.seq = r.entry LEFT JOIN compaction c ON c.entry = r.checkpoint",
        |row| {
            let seq = row.get::<i64>(0)?;
            let checkpoint = row.get::<Option<i64>>(3)?;
            let (through, frontier) = (row.get::<Option<u64>>(5)?, row.get::<u64>(7)?);
            let kept = checkpoint.and_then(|checkpoint| retained.get(&checkpoint));
            let later = commits
                .get(&row.get::<i64>(6)?)
                .map_or(&[][..], Vec::as_slice);
            let later = later
                .iter()
                .filter(|&&source| source > frontier && Some(source) <= through);
            let omitted = omitted.get(&seq).map_or(&[][..], Vec::as_slice);
            SessionEvent::ModelRequested {
                context: row.get(1)?,
                history: checkpoint
                    .map(u64_of)
                    .into_iter()
                    .chain(kept.into_iter().flatten().chain(later).copied())
                    .filter(|source| !omitted.contains(source))
                    .collect(),
                tail: tails.get(&seq).cloned().unwrap_or_default(),
                history_lifetime: parse_variant(row.get(4)?)?,
                purpose: parse_variant(row.get(2)?)?,
            }
        }
    );
    load!(
        "SELECT c.entry, c.schema_version, a.request, a.attempt, c.frontier, c.message, \
         c.before_tokens, c.after_tokens FROM compaction c \
         JOIN attempt_outcome o ON o.entry = c.entry JOIN model_attempt a ON a.entry = o.attempt",
        |row| {
            let seq = row.get::<i64>(0)?;
            SessionEvent::Compaction {
                checkpoint: CompactionCheckpoint {
                    schema_version: u16::try_from(row.get::<u32>(1)?)
                        .map_err(|_| corrupt("schema version overflow"))?,
                    // Linked in sequence order below.
                    previous: None,
                    frontier: row.get(4)?,
                    message: messages.message(row.get(5)?)?,
                    todos: todos.get(&seq).cloned().unwrap_or_default(),
                    retained: retained.get(&seq).cloned().unwrap_or_default(),
                    request: row.get(2)?,
                    attempt: row.get(3)?,
                    before_tokens: row.get(6)?,
                    after_tokens: row.get(7)?,
                },
            }
        }
    );
    load!("SELECT entry, request, attempt FROM model_attempt", |row| {
        SessionEvent::ModelAttemptStarted {
            request: row.get(1)?,
            attempt: row.get(2)?,
        }
    });
    load!(
        "SELECT f.entry, a.request, a.attempt, f.error, f.failure FROM model_failure f \
         JOIN attempt_outcome o ON o.entry = f.entry JOIN model_attempt a ON a.entry = o.attempt",
        |row| SessionEvent::ModelFailed {
            request: row.get(1)?,
            attempt: row.get(2)?,
            error: row.get(3)?,
            kind: parse_variant(row.get(4)?)?,
        }
    );
    load!(
        "SELECT o.entry, a.request, a.attempt FROM attempt_outcome o \
         JOIN model_attempt a ON a.entry = o.attempt WHERE o.kind = 'model_attempt_interrupted'",
        |row| SessionEvent::ModelAttemptInterrupted {
            request: row.get(1)?,
            attempt: row.get(2)?,
        }
    );
    load!(
        "SELECT r.entry, a.request, a.attempt, m.entry, r.stop_reason, r.stop_other \
         FROM model_response r JOIN attempt_outcome o ON o.entry = r.entry \
         JOIN model_attempt a ON a.entry = o.attempt \
         LEFT JOIN message_commit m ON m.message = r.message",
        |row| SessionEvent::ResponseCompleted {
            request: row.get(1)?,
            attempt: row.get(2)?,
            message: row.get(3)?,
            stop_reason: match (row.get::<String>(4)?, row.get(5)?) {
                (reason, Some(other)) if reason == "other" => StopReason::Other(other),
                (reason, _) => parse_variant(reason)?,
            },
        }
    );
    load!(
        "SELECT v.entry, a.request, a.attempt, v.delay_millis, f.error \
         FROM model_recovery v JOIN model_failure f ON f.entry = v.failure \
         JOIN attempt_outcome o ON o.entry = f.entry JOIN model_attempt a ON a.entry = o.attempt",
        |row| SessionEvent::ModelRecoveryScheduled {
            request: row.get(1)?,
            attempt: row.get::<u64>(2)?.saturating_add(1),
            delay_millis: row.get(3)?,
            error: row.get(4)?,
        }
    );
    load!(
        "SELECT o.entry, o.kind, o.request, a.attempt, o.reason FROM compaction_outcome o \
         LEFT JOIN model_attempt a ON a.entry = o.attempt",
        |row| {
            let (request, attempt, reason) = (
                row.get::<Option<u64>>(2)?,
                row.get::<Option<u64>>(3)?,
                row.get(4)?,
            );
            match row.get::<String>(1)?.as_str() {
                "compaction_skipped" => SessionEvent::CompactionSkipped {
                    request: request.ok_or_else(|| corrupt("skipped compaction has no request"))?,
                    attempt: attempt.ok_or_else(|| corrupt("skipped compaction has no attempt"))?,
                    reason,
                },
                _ => SessionEvent::CompactionFailed {
                    request,
                    attempt,
                    error: reason,
                },
            }
        }
    );
    load!(
        "SELECT entry, request, input_tokens, cached_input_tokens, output_tokens FROM usage",
        |row| SessionEvent::Usage {
            request: row.get(1)?,
            usage: Usage {
                input_tokens: row.get(2)?,
                cached_input_tokens: row.get(3)?,
                output_tokens: row.get(4)?,
            },
        }
    );
    load!(
        "SELECT j.created, j.id, j.parent, j.origin_call, m.entry, c.call_id, j.tool, j.name, \
         j.role, j.arguments, j.output_schema, j.accepts_input, j.background, t.name, \
         j.location_workspace, j.authorization_scope FROM job j \
         JOIN target t ON t.id = j.location_target \
         LEFT JOIN tool_call c ON c.block = j.origin_call \
         LEFT JOIN assistant_block b ON b.id = c.block \
         LEFT JOIN assistant_item i ON i.id = b.item \
         LEFT JOIN message_commit m ON m.message = i.message",
        |row| {
            let origin = match (row.get::<Option<i64>>(3)?, row.get(4)?, row.get(5)?) {
                (None, _, _) => None,
                (Some(_), Some(message), Some(call_id)) => {
                    Some(ModelCallOrigin { message, call_id })
                }
                _ => return Err(corrupt("job origin call is not committed")),
            };
            SessionEvent::JobCreated {
                job: job(row.get(1)?)?,
                parent: row.get::<Option<i64>>(2)?.map(job).transpose()?,
                origin,
                tool: row.get(6)?,
                role: parse_variant(row.get(8)?)?,
                name: row.get(7)?,
                arguments: parse_json(&row.get::<String>(9)?)?,
                output_schema: row
                    .get::<Option<String>>(10)?
                    .as_deref()
                    .map(parse_json)
                    .transpose()?,
                accepts_input: row.get(11)?,
                background: row.get(12)?,
                authorization_scope: row.get(15)?,
                location: location(row.get(13)?, row.get(14)?),
            }
        }
    );
    load!(
        "SELECT entry, capability, resource, coverage FROM approval_grant",
        |row| SessionEvent::ApprovalGranted {
            grant: crate::tool::policy::ApprovalGrant {
                capability: capability(row.get(1)?)?,
                resource: parse_json(&row.get::<String>(2)?)?,
                coverage: parse_variant(row.get(3)?)?,
            },
        }
    );
    load!(
        "SELECT entry, grant_entry FROM approval_revocation",
        |row| { SessionEvent::ApprovalRevoked { grant: row.get(1)? } }
    );
    load!("SELECT entry, job, state FROM job_transition", |row| {
        SessionEvent::JobStateChanged {
            job: job(row.get(1)?)?,
            state: parse_variant(row.get(2)?)?,
        }
    });
    load!(
        "SELECT entry, job, state, error, denial_code, denial_executed FROM job_finish",
        |row| SessionEvent::JobFinished {
            job: job(row.get(1)?)?,
            state: parse_variant(row.get(2)?)?,
            error: row.get(3)?,
            images: finish_images
                .get(&row.get::<i64>(0)?)
                .cloned()
                .unwrap_or_default(),
            denial: match (row.get::<Option<String>>(4)?, row.get(5)?) {
                (Some(code), Some(executed)) => Some(Denial {
                    code: parse_variant(code)?,
                    executed,
                }),
                _ => None,
            },
        }
    );
    load!(
        "SELECT entry, kind, job, notification, source FROM job_delivery",
        |row| {
            let job = job(row.get(2)?)?;
            let notification = row.get::<Option<u64>>(3)?;
            let missing = || corrupt("message delivery has no notification or source");
            match row.get::<String>(1)?.as_str() {
                "job_claimed" => SessionEvent::JobClaimed { job },
                "job_injected" => SessionEvent::JobInjected { job },
                _ => SessionEvent::JobMessageDelivered {
                    job,
                    source: row.get::<Option<u64>>(4)?.ok_or_else(missing)?,
                    notification: notification.ok_or_else(missing)?,
                },
            }
        }
    );
    let public_id = db.query_row("SELECT public_id FROM session", Vec::new(), |row| {
        Ok(row.get::<Vec<u8>>(0)?)
    })?;
    if public_id.is_some_and(|id| id.as_slice() != session.to_bytes()) {
        return Err(corrupt("database belongs to another session"));
    }
    let session_capabilities = sorted(db.query(
        "SELECT capability FROM session_capability",
        Vec::new(),
        |row| capability(row.get(0)?),
    )?);
    let entries = db.query(
        "SELECT seq, public_id, agent, created_millis, kind FROM entry ORDER BY seq",
        Vec::new(),
        |row| {
            Ok((
                row.get::<i64>(0)?,
                row.get::<Vec<u8>>(1)?,
                row.get::<i64>(2)?,
                row.get::<i64>(3)?,
                row.get::<String>(4)?,
            ))
        },
    )?;
    let mut last_compaction: HashMap<i64, u64> = HashMap::new();
    let mut records = Vec::with_capacity(entries.len());
    for (seq, public_id, agent, created, kind) in entries {
        let mut event = match kind.as_str() {
            "session_started" => SessionEvent::SessionStarted {
                targets: targets.get(&seq).cloned().unwrap_or_default(),
                capabilities: session_capabilities.clone(),
            },
            "targets_upserted" => SessionEvent::TargetsUpserted {
                targets: targets.get(&seq).cloned().unwrap_or_default(),
            },
            "todos_replaced" => SessionEvent::TodosReplaced {
                items: todos.get(&seq).cloned().unwrap_or_default(),
            },
            "agent_completed" => SessionEvent::AgentCompleted,
            "agent_interrupted" => SessionEvent::AgentInterrupted,
            _ => events
                .remove(&seq)
                .ok_or_else(|| corrupt(format!("entry {seq} ({kind}) has no {kind} row")))?,
        };
        if let SessionEvent::Compaction { checkpoint } = &mut event {
            checkpoint.previous = last_compaction.insert(agent, u64_of(seq));
        }
        records.push(EventRecord {
            id: event_id(public_id)?,
            sequence: u64_of(seq),
            timestamp_millis: created,
            agent: agent_of(agent)?,
            event,
        });
    }
    Ok(records)
}

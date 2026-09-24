//! Reassemble session events from normalized rows.

use std::collections::HashMap;

use libsql::Row;
use serde::de::DeserializeOwned;

use super::{
    Db, DbResult, JobEventKind, MessageRole, ResponseOutcome, UserPartKind, corrupt, diagnostic,
    enum_column, optional_enum_column,
};
use crate::{
    agent::TodoItem,
    execution::ExecutionLocation,
    identity::{AgentId, EventId, JobId, SessionId},
    job::{AgentMessage, AgentProgress},
    media::{AttachmentRef, BlobDigest, BlobRef, ImageFormat, ImageRef, TextRef},
    provider::{
        profile::ModelProfile,
        protocol::{
            AssistantItem, ItemId, ItemKind, Position, Provenance, Replay, ResponseSchema,
            SystemSegment, TextBlock, ToolCall, ToolDefinition, ToolResult, Usage,
        },
    },
    session::{
        AttemptRef, CompactionCheckpoint, CompactionFailure, CompletedOutcome, EntryKind,
        EventRecord, JobEvent, Message, ModelCallOrigin, ModelContext, ProfileSnapshot, RecordSeq,
        RuntimeState, SessionEvent, StateJob, StateJobKind, UserPart,
    },
    target::{SshAuth, SshOptions, TargetAuth, TargetDefinition, TargetName, TargetRef},
    tool::{
        policy::{ApprovalGrant, Capability, ResourceId, ResourceKind},
        registry::JobName,
    },
};

fn parse_json<T: DeserializeOwned>(text: &str) -> DbResult<T> {
    serde_json::from_str(text).map_err(|error| corrupt(error.to_string()))
}

/// A nonblank identity newtype (`ItemId`, `BlockId`, `Scope`) from its column.
fn parsed<T>(text: String) -> DbResult<T>
where
    T: TryFrom<String>,
    T::Error: std::fmt::Display,
{
    T::try_from(text).map_err(|error| corrupt(error.to_string()))
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

/// A journal sequence read back from its row.
pub(super) fn sequence(value: i64) -> RecordSeq {
    RecordSeq::new(u64_of(value))
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

/// part, kind, text, blob (digest, length), image format, file name
type UserPartRow = (
    i64,
    UserPartKind,
    Option<String>,
    Option<(Vec<u8>, u64)>,
    Option<ImageFormat>,
    Option<String>,
);
/// position, provider id, text
type Block = (i64, String, String);
/// A state job row before its children are attached: position, parent position, job.
type StateJobRow = (u64, Option<u64>, StateJob);

struct Messages {
    roles: HashMap<i64, MessageRole>,
    parts: HashMap<i64, Vec<UserPartRow>>,
    /// date, location, per state part
    states: HashMap<i64, (String, ExecutionLocation)>,
    state_jobs: HashMap<i64, Vec<StateJobRow>>,
    state_todos: HashMap<i64, Vec<TodoItem>>,
    job_events: HashMap<i64, Vec<JobEvent>>,
    items: HashMap<i64, Vec<(i64, i64, String, ItemKind)>>,
    replays: HashMap<i64, Replay>,
    blocks: HashMap<i64, Vec<Block>>,
    /// call id, name, arguments
    calls: HashMap<i64, (String, String, String)>,
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
        format: enum_column(row, at + 2)?,
        file: row.get(at + 3)?,
    })
}

fn position(value: i64) -> DbResult<Position> {
    u32::try_from(value)
        .map(Position::from)
        .map_err(|_| corrupt("position does not fit"))
}

/// The jobs under `parent`, in position order, with their own children attached.
fn state_tree(rows: &[StateJobRow], parent: Option<u64>) -> Vec<StateJob> {
    rows.iter()
        .filter(|(_, above, _)| *above == parent)
        .map(|(position, _, job)| StateJob {
            children: state_tree(rows, Some(*position)),
            ..job.clone()
        })
        .collect()
}

fn job_event(row: &Row) -> DbResult<JobEvent> {
    Ok(match enum_column(row, 1)? {
        JobEventKind::Message => JobEvent::Message(AgentMessage {
            id: job(row.get(2)?)?,
            name: row.get(3)?,
            message: row
                .get::<Option<i64>>(4)?
                .map(|entry| sequence(entry).message())
                .ok_or_else(|| corrupt("child message has no source"))?,
            text: row
                .get::<Option<String>>(5)?
                .ok_or_else(|| corrupt("child message has no text"))?,
        }),
        JobEventKind::Job => JobEvent::Job(Box::new(parse_json(
            &row.get::<Option<String>>(6)?
                .ok_or_else(|| corrupt("job view has no document"))?,
        )?)),
    })
}

impl Messages {
    fn load(db: &Db) -> DbResult<Self> {
        Ok(Self {
            roles: keyed(db, "SELECT id, role FROM message", |row| {
                enum_column(row, 1)
            })?,
            parts: grouped(
                db,
                "SELECT p.message, p.id, p.kind, p.text, p.blob, length(b.bytes), \
                 p.image_format, p.file FROM user_part p LEFT JOIN blob b ON b.sha256 = p.blob \
                 ORDER BY p.message, p.position",
                |row| {
                    let blob = match row.get::<Option<Vec<u8>>>(4)? {
                        Some(digest) => Some((digest, row.get(5)?)),
                        None => None,
                    };
                    Ok((
                        row.get(1)?,
                        enum_column(row, 2)?,
                        row.get(3)?,
                        blob,
                        optional_enum_column(row, 6)?,
                        row.get(7)?,
                    ))
                },
            )?,
            states: keyed(
                db,
                "SELECT s.part, s.date, t.name, s.location_workspace FROM user_part_state s \
                 JOIN target t ON t.id = s.location_target",
                |row| {
                    let location = ExecutionLocation {
                        target: target_ref(row.get(2)?)?,
                        workspace: path(row.get(3)?),
                    };
                    Ok((row.get(1)?, location))
                },
            )?,
            state_jobs: grouped(
                db,
                "SELECT j.part, j.position, j.parent_position, j.job, j.tool, j.name, j.state, \
                 t.name, j.workspace, j.age_seconds, j.turns, j.tool_calls \
                 FROM user_part_state_job j LEFT JOIN target t ON t.id = j.target \
                 ORDER BY j.part, j.position",
                |row| {
                    let tool: String = row.get(4)?;
                    let progress = (row.get::<Option<u64>>(10)?, row.get::<Option<u64>>(11)?);
                    let kind = match (tool.as_str(), progress) {
                        (StateJobKind::AGENT, (Some(turns), Some(tool_calls))) => {
                            StateJobKind::Agent {
                                progress: AgentProgress { turns, tool_calls },
                            }
                        }
                        (_, (None, None)) if tool != StateJobKind::AGENT => {
                            StateJobKind::Tool { tool }
                        }
                        _ => return Err(corrupt("state job progress does not match its tool")),
                    };
                    Ok((
                        row.get::<u64>(1)?,
                        row.get::<Option<u64>>(2)?,
                        StateJob {
                            job: job(row.get(3)?)?,
                            kind,
                            name: row.get(5)?,
                            state: enum_column(row, 6)?,
                            target: row.get::<Option<String>>(7)?.map(target_ref).transpose()?,
                            workspace: path(row.get(8)?),
                            age_seconds: row.get(9)?,
                            children: Vec::new(),
                        },
                    ))
                },
            )?,
            state_todos: grouped(
                db,
                "SELECT part, text, status FROM user_part_state_todo ORDER BY part, position",
                |row| {
                    Ok(TodoItem {
                        text: row.get(1)?,
                        status: enum_column(row, 2)?,
                    })
                },
            )?,
            job_events: grouped(
                db,
                "SELECT e.part, e.kind, e.job, e.name, e.source, e.text, e.view \
                 FROM user_part_job_event e ORDER BY e.part, e.position",
                job_event,
            )?,
            items: grouped(
                db,
                "SELECT message, id, position, provider_id, kind FROM assistant_item \
                 ORDER BY message, position",
                |row| Ok((row.get(1)?, row.get(2)?, row.get(3)?, enum_column(row, 4)?)),
            )?,
            replays: keyed(
                db,
                "SELECT item, protocol, model, scope, payload, binding FROM reasoning_replay",
                |row| {
                    Ok(Replay {
                        provenance: Provenance {
                            protocol: row.get(1)?,
                            model: row.get(2)?,
                            scope: parsed(row.get(3)?)?,
                        },
                        payload: parse_json(&row.get::<String>(4)?)?,
                        binding: enum_column(row, 5)?,
                    })
                },
            )?,
            blocks: grouped(
                db,
                "SELECT item, position, provider_id, text FROM assistant_block \
                 ORDER BY item, position",
                |row| Ok((row.get(1)?, row.get(2)?, row.get(3)?)),
            )?,
            calls: keyed(
                db,
                "SELECT item, call_id, name, arguments FROM tool_call",
                |row| Ok((row.get(1)?, row.get(2)?, row.get(3)?)),
            )?,
            results: keyed(
                db,
                "SELECT r.message, c.call_id, c.name, r.result, r.is_error FROM tool_result r \
                 JOIN tool_call c ON c.item = r.call",
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
        Ok(match role {
            MessageRole::User => {
                let mut content = Vec::new();
                for (part, kind, text, blob, format, file) in
                    self.parts.get(&id).into_iter().flatten()
                {
                    let text = || text.clone().ok_or_else(|| corrupt("text part has no text"));
                    content.push(match kind {
                        UserPartKind::Text => UserPart::Text { text: text()? },
                        UserPartKind::ParentInput => UserPart::ParentInput { text: text()? },
                        UserPartKind::Compaction => UserPart::Compaction { text: text()? },
                        UserPartKind::State => {
                            let (date, location) = self
                                .states
                                .get(part)
                                .cloned()
                                .ok_or_else(|| corrupt("state part has no state"))?;
                            let jobs = self.state_jobs.get(part).map_or(&[][..], Vec::as_slice);
                            UserPart::State {
                                state: RuntimeState {
                                    date,
                                    jobs: state_tree(jobs, None),
                                    todos: self.state_todos.get(part).cloned().unwrap_or_default(),
                                    location,
                                },
                            }
                        }
                        UserPartKind::JobEvents => UserPart::JobEvents {
                            events: self.job_events.get(part).cloned().unwrap_or_default(),
                        },
                        UserPartKind::Attachment => {
                            let (digest, bytes) = blob
                                .clone()
                                .ok_or_else(|| corrupt("attachment has no blob"))?;
                            let (blob, file) = (self::blob(digest, bytes)?, file.clone());
                            let attachment = match format {
                                Some(format) => AttachmentRef::Image(ImageRef {
                                    file,
                                    format: *format,
                                    blob,
                                }),
                                None => AttachmentRef::Text(TextRef { file, blob }),
                            };
                            UserPart::Attachment { attachment }
                        }
                    });
                }
                Message::User(content)
            }
            MessageRole::Assistant => {
                let mut items = Vec::new();
                for (item, item_position, provider_id, kind) in
                    self.items.get(&id).into_iter().flatten()
                {
                    let id: ItemId = parsed(provider_id.clone())?;
                    let position = position(*item_position)?;
                    let blocks = || {
                        self.blocks
                            .get(item)
                            .into_iter()
                            .flatten()
                            .map(|(block_position, block_id, text)| {
                                Ok(TextBlock {
                                    id: parsed(block_id.clone())?,
                                    position: self::position(*block_position)?,
                                    text: text.clone(),
                                })
                            })
                            .collect::<DbResult<Vec<_>>>()
                    };
                    items.push(match (kind, self.calls.get(item)) {
                        (ItemKind::Text, None) => AssistantItem::Text {
                            id,
                            position,
                            blocks: blocks()?,
                        },
                        (ItemKind::Reasoning, None) => AssistantItem::Reasoning {
                            id,
                            position,
                            blocks: blocks()?,
                            replay: self.replays.get(item).cloned(),
                        },
                        (ItemKind::ToolCall, Some((call_id, name, arguments))) => {
                            AssistantItem::ToolCall {
                                id,
                                position,
                                call: ToolCall::new(
                                    call_id.clone(),
                                    name.clone(),
                                    parse_json(arguments)?,
                                )
                                .map_err(|error| corrupt(error.to_string()))?,
                            }
                        }
                        _ => return Err(corrupt("assistant item does not match its rows")),
                    });
                }
                Message::Assistant(items)
            }
            MessageRole::Tool => {
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
        })
    }
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
         state_mode, hint FROM model_profile",
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
                    state_mode: enum_column(row, 8)?,
                    hint: row.get(9)?,
                },
            })
        },
    )
}

/// Journaled target names were validated when written; anything else is corruption.
pub(super) fn target_ref(name: String) -> DbResult<TargetRef> {
    TargetRef::try_from(name).map_err(|error| corrupt(error.to_string()))
}

fn target_name(name: String) -> DbResult<TargetName> {
    TargetName::try_from(name).map_err(|error| corrupt(error.to_string()))
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
            let auth = match (enum_column(row, 9)?, row.get::<Option<Vec<u8>>>(10)?) {
                (SshAuth::Default, None) => TargetAuth::Default,
                (SshAuth::Agent, None) => TargetAuth::Agent,
                (SshAuth::Key, Some(key)) => TargetAuth::Key { path: path(key) },
                _ => return Err(corrupt("target authentication is inconsistent")),
            };
            let port = row.get::<Option<u32>>(8)?;
            Ok((
                row.get::<i64>(0)?,
                TargetDefinition {
                    name: target_name(row.get(2)?)?,
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
                    via: row
                        .get::<Option<String>>(12)?
                        .map(target_name)
                        .transpose()?,
                    origin: row
                        .get::<Option<String>>(13)?
                        .map(target_name)
                        .transpose()?,
                    source: enum_column(row, 4)?,
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
    let diagnostics = diagnostic::load(db)?;
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
                status: enum_column(row, 2)?,
            })
        },
    )?;
    let location = |target: String, workspace: Vec<u8>| {
        DbResult::Ok(ExecutionLocation {
            target: target_ref(target)?,
            workspace: path(workspace),
        })
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
        |row| enum_column(row, 1),
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

    let mode_capabilities = grouped(db, "SELECT mode, capability FROM mode_capability", |row| {
        enum_column(row, 1)
    })?;
    // Mode definitions by the entry that pinned them.
    let pinned_modes = db.query(
        "SELECT id, name, instructions, hint, entry FROM mode",
        Vec::new(),
        |row| {
            let capabilities = mode_capabilities.get(&row.get::<i64>(0)?);
            let mode = crate::tool::policy::Mode {
                capabilities: sorted(capabilities.cloned().unwrap_or_default()),
                instructions: row.get(2)?,
                hint: row.get(3)?,
            };
            Ok((row.get::<i64>(4)?, mode))
        },
    )?;
    let pinned_modes: HashMap<i64, _> = pinned_modes.into_iter().collect();
    let selection = |entry: i64, name: Option<String>| {
        name.map(|name| crate::session::ModeSelection {
            name,
            definition: pinned_modes.get(&entry).cloned(),
        })
    };
    // Every other entry's event, built by its subtype's loader.
    let mut events: HashMap<i64, SessionEvent> = HashMap::new();
    macro_rules! load {
        ($sql:expr, |$row:ident| $event:expr) => {
            events.extend(keyed(db, $sql, |$row| Ok($event))?)
        };
    }
    load!("SELECT entry, kind, text FROM entry_text", |row| {
        let text = row.get(2)?;
        match enum_column(row, 1)? {
            EntryKind::TitleSet => SessionEvent::TitleSet { title: text },
            EntryKind::Status => SessionEvent::Status { message: text },
            EntryKind::AgentFailed => SessionEvent::AgentFailed { error: text },
            kind => return Err(corrupt(format!("{kind} entry has a text row"))),
        }
    });
    let capabilities_at = |entry: i64| {
        let capabilities = agent_capabilities.get(&entry);
        sorted(capabilities.cloned().unwrap_or_default())
    };
    load!(
        "SELECT s.entry, a.id, a.parent, a.owner_job, a.available_depth, t.name, \
         s.location_workspace, s.profile, m.name FROM agent_start s \
         JOIN entry e ON e.seq = s.entry JOIN agent a ON a.id = e.agent \
         JOIN target t ON t.id = s.location_target \
         LEFT JOIN agent_mode am ON am.entry = s.entry LEFT JOIN mode m ON m.id = am.mode",
        |row| SessionEvent::AgentStarted {
            owner_job: row.get::<Option<i64>>(3)?.map(job).transpose()?,
            profile: row.get::<Option<i64>>(7)?.map(profile).transpose()?,
            available_depth: row.get(4)?,
            mode: selection(row.get(0)?, row.get(8)?),
            capabilities: capabilities_at(row.get(0)?),
            location: location(row.get(5)?, row.get(6)?)?,
        }
    );
    load!(
        "SELECT am.entry, m.name FROM agent_mode am JOIN mode m ON m.id = am.mode \
         WHERE am.kind = 'mode_changed'",
        |row| SessionEvent::ModeChanged {
            mode: selection(row.get(0)?, row.get(1)?)
                .ok_or_else(|| corrupt("mode_changed entry has no mode"))?,
            capabilities: capabilities_at(row.get(0)?),
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
                    purpose: enum_column(row, 1)?,
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
        "SELECT r.entry, r.context, r.checkpoint, r.history_lifetime, r.history_through, \
         e.agent, coalesce(c.frontier, 0) FROM model_request r \
         JOIN entry e ON e.seq = r.entry LEFT JOIN compaction c ON c.entry = r.checkpoint",
        |row| {
            let seq = row.get::<i64>(0)?;
            let checkpoint = row.get::<Option<i64>>(2)?;
            let (through, frontier) = (row.get::<Option<u64>>(4)?, row.get::<u64>(6)?);
            let kept = checkpoint.and_then(|checkpoint| retained.get(&checkpoint));
            let later = commits
                .get(&row.get::<i64>(5)?)
                .map_or(&[][..], Vec::as_slice);
            let later = later
                .iter()
                .filter(|&&source| source > frontier && Some(source) <= through);
            let omitted = omitted.get(&seq).map_or(&[][..], Vec::as_slice);
            SessionEvent::ModelRequested {
                context: sequence(row.get(1)?),
                checkpoint: checkpoint.map(sequence),
                history: kept
                    .into_iter()
                    .flatten()
                    .chain(later)
                    .copied()
                    .filter(|source| !omitted.contains(source))
                    .map(|source| RecordSeq::new(source).message())
                    .collect(),
                tail: tails.get(&seq).cloned().unwrap_or_default(),
                history_lifetime: enum_column(row, 3)?,
            }
        }
    );
    let attempt = |row: &Row, at: i32| -> DbResult<AttemptRef> {
        Ok(AttemptRef {
            request: sequence(row.get(at)?).request(),
            attempt: row.get(at + 1)?,
        })
    };
    load!(
        "SELECT c.entry, a.request, a.attempt, c.frontier, c.message, c.before_tokens, \
         c.after_tokens FROM compaction c \
         JOIN attempt_outcome o ON o.entry = c.entry JOIN model_attempt a ON a.entry = o.attempt",
        |row| {
            let seq = row.get::<i64>(0)?;
            SessionEvent::Compaction {
                checkpoint: CompactionCheckpoint {
                    frontier: sequence(row.get(3)?),
                    message: messages.message(row.get(4)?)?,
                    todos: todos.get(&seq).cloned().unwrap_or_default(),
                    retained: retained
                        .get(&seq)
                        .map(|retained| {
                            retained
                                .iter()
                                .map(|source| RecordSeq::new(*source).message())
                                .collect()
                        })
                        .unwrap_or_default(),
                    attempt: attempt(row, 1)?,
                    before_tokens: row.get(5)?,
                    after_tokens: row.get(6)?,
                },
            }
        }
    );
    load!("SELECT entry, request, attempt FROM model_attempt", |row| {
        SessionEvent::ModelAttemptStarted(attempt(row, 1)?)
    });
    load!(
        "SELECT f.entry, a.request, a.attempt, f.error, f.failure FROM model_failure f \
         JOIN attempt_outcome o ON o.entry = f.entry JOIN model_attempt a ON a.entry = o.attempt",
        |row| SessionEvent::ModelFailed {
            attempt: attempt(row, 1)?,
            error: row.get(3)?,
            kind: enum_column(row, 4)?,
        }
    );
    load!(
        "SELECT o.entry, a.request, a.attempt FROM attempt_outcome o \
         JOIN model_attempt a ON a.entry = o.attempt WHERE o.kind = 'model_attempt_interrupted'",
        |row| SessionEvent::ModelAttemptInterrupted(attempt(row, 1)?)
    );
    load!(
        "SELECT r.entry, a.request, a.attempt, m.entry, r.outcome, r.cut_reason \
         FROM model_response r JOIN attempt_outcome o ON o.entry = r.entry \
         JOIN model_attempt a ON a.entry = o.attempt \
         JOIN message_commit m ON m.message = r.message",
        |row| SessionEvent::ResponseCompleted {
            attempt: attempt(row, 1)?,
            message: sequence(row.get(3)?).message(),
            outcome: match (enum_column(row, 4)?, optional_enum_column(row, 5)?) {
                (ResponseOutcome::Answer, None) => CompletedOutcome::Answer,
                (ResponseOutcome::ToolUse, None) => CompletedOutcome::ToolUse,
                (ResponseOutcome::Cut, Some(truncation)) => CompletedOutcome::Cut(truncation),
                _ => return Err(corrupt("response outcome does not match its cut reason")),
            },
        }
    );
    load!(
        "SELECT entry, failure, delay_millis FROM model_recovery",
        |row| SessionEvent::ModelRecoveryScheduled {
            failure: sequence(row.get(1)?),
            delay_millis: row.get(2)?,
        }
    );
    load!(
        "SELECT o.entry, o.kind, o.request, a.attempt, o.reason FROM compaction_outcome o \
         LEFT JOIN model_attempt a ON a.entry = o.attempt",
        |row| {
            let reason = row.get(4)?;
            let request = row
                .get::<Option<i64>>(2)?
                .map(|request| sequence(request).request());
            let attempt = match (request, row.get::<Option<u64>>(3)?) {
                (Some(request), Some(attempt)) => Some(AttemptRef { request, attempt }),
                (_, None) => None,
                (None, Some(_)) => return Err(corrupt("compaction attempt has no request")),
            };
            match enum_column(row, 1)? {
                EntryKind::CompactionSkipped => SessionEvent::CompactionSkipped {
                    attempt: attempt.ok_or_else(|| corrupt("skipped compaction has no attempt"))?,
                    reason,
                },
                EntryKind::CompactionFailed => SessionEvent::CompactionFailed {
                    failure: match (request, attempt) {
                        (None, _) => CompactionFailure::BeforeRequest,
                        (Some(request), None) => CompactionFailure::Requested(request),
                        (Some(_), Some(attempt)) => CompactionFailure::Attempted(attempt),
                    },
                    error: reason,
                },
                kind => return Err(corrupt(format!("{kind} entry has a compaction outcome"))),
            }
        }
    );
    load!(
        "SELECT entry, request, input_tokens, cached_input_tokens, output_tokens FROM usage",
        |row| SessionEvent::Usage {
            request: sequence(row.get(1)?).request(),
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
         j.location_workspace FROM job j \
         JOIN target t ON t.id = j.location_target \
         LEFT JOIN tool_call c ON c.item = j.origin_call \
         LEFT JOIN assistant_item i ON i.id = c.item \
         LEFT JOIN message_commit m ON m.message = i.message",
        |row| {
            let origin = match (
                row.get::<Option<i64>>(3)?,
                row.get::<Option<i64>>(4)?,
                row.get(5)?,
            ) {
                (None, _, _) => None,
                (Some(_), Some(message), Some(call_id)) => Some(ModelCallOrigin {
                    message: sequence(message).message(),
                    call_id,
                }),
                _ => return Err(corrupt("job origin call is not committed")),
            };
            SessionEvent::JobCreated {
                job: job(row.get(1)?)?,
                parent: row.get::<Option<i64>>(2)?.map(job).transpose()?,
                origin,
                tool: row.get(6)?,
                role: enum_column(row, 8)?,
                name: row
                    .get::<Option<String>>(7)?
                    .map(JobName::try_from)
                    .transpose()
                    .map_err(|_| corrupt("job name is invalid"))?,
                arguments: parse_json(&row.get::<String>(9)?)?,
                output_schema: row
                    .get::<Option<String>>(10)?
                    .as_deref()
                    .map(parse_json)
                    .transpose()?,
                accepts_input: row.get(11)?,
                background: row.get(12)?,
                location: location(row.get(13)?, row.get(14)?)?,
            }
        }
    );
    let path_components = grouped(
        db,
        "SELECT grant_entry, component FROM approval_grant_path_component \
         ORDER BY grant_entry, position",
        |row| Ok(row.get::<String>(1)?),
    )?;
    let route_hops = grouped(
        db,
        "SELECT h.grant_entry, t.name, h.revision FROM approval_grant_route_hop h \
         JOIN target t ON t.id = h.target ORDER BY h.grant_entry, h.position",
        |row| Ok((row.get::<String>(1)?, row.get::<u64>(2)?)),
    )?;
    load!(
        "SELECT g.entry, g.capability, g.resource_kind, t.name, g.path, g.origin, \
         g.session_name, g.mcp_server, g.mcp_tool, g.coverage FROM approval_grant g \
         LEFT JOIN target t ON t.id = g.target",
        |row| {
            let seq = row.get::<i64>(0)?;
            let text = |index: i32| -> DbResult<String> {
                row.get::<Option<String>>(index)?
                    .ok_or_else(|| corrupt("approval grant resource column is missing"))
            };
            let resource = match enum_column(row, 2)? {
                ResourceKind::Workspace => ResourceId::Workspace {
                    target: text(3)?,
                    path: text(4)?,
                },
                ResourceKind::Path => ResourceId::Path {
                    target: text(3)?,
                    components: path_components.get(&seq).cloned().unwrap_or_default(),
                },
                ResourceKind::Network => ResourceId::Network {
                    target: text(3)?,
                    origin: text(5)?,
                },
                ResourceKind::Route => ResourceId::Route {
                    destination: text(3)?,
                    hops: route_hops.get(&seq).cloned().unwrap_or_default(),
                },
                ResourceKind::Session => ResourceId::Session { name: text(6)? },
                ResourceKind::Mcp => ResourceId::Mcp {
                    server: text(7)?,
                    tool: text(8)?,
                },
            };
            SessionEvent::ApprovalGranted {
                grant: ApprovalGrant {
                    capability: enum_column(row, 1)?,
                    resource,
                    coverage: enum_column(row, 9)?,
                },
            }
        }
    );
    load!(
        "SELECT entry, grant_entry FROM approval_revocation",
        |row| {
            SessionEvent::ApprovalRevoked {
                grant: sequence(row.get(1)?),
            }
        }
    );
    load!("SELECT entry, job, state FROM job_transition", |row| {
        SessionEvent::JobStateChanged {
            job: job(row.get(1)?)?,
            state: enum_column(row, 2)?,
        }
    });
    load!("SELECT entry, job, state FROM job_finish", |row| {
        SessionEvent::JobFinished {
            job: job(row.get(1)?)?,
            state: enum_column(row, 2)?,
            diagnostic: diagnostics
                .get(&(row.get::<i64>(0)?, diagnostic::Slot::Diagnostic))
                .cloned(),
            output_diagnostic: diagnostics
                .get(&(row.get::<i64>(0)?, diagnostic::Slot::OutputDiagnostic))
                .cloned(),
            images: finish_images
                .get(&row.get::<i64>(0)?)
                .cloned()
                .unwrap_or_default(),
        }
    });
    load!(
        "SELECT entry, kind, job, notification, source FROM job_delivery",
        |row| {
            let job = job(row.get(2)?)?;
            let notification = row
                .get::<Option<i64>>(3)?
                .map(|entry| sequence(entry).message());
            let missing = || corrupt("message delivery has no notification or source");
            match enum_column(row, 1)? {
                EntryKind::JobClaimed => SessionEvent::JobClaimed { job },
                EntryKind::JobInjected => SessionEvent::JobInjected { job },
                EntryKind::JobMessageDelivered => SessionEvent::JobMessageDelivered {
                    job,
                    source: row
                        .get::<Option<i64>>(4)?
                        .map(|entry| sequence(entry).message())
                        .ok_or_else(missing)?,
                    notification: notification.ok_or_else(missing)?,
                },
                kind => return Err(corrupt(format!("{kind} entry has a delivery row"))),
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
        |row| enum_column(row, 0),
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
                enum_column::<EntryKind>(row, 4)?,
            ))
        },
    )?;
    let mut records = Vec::with_capacity(entries.len());
    for (seq, public_id, agent, created, kind) in entries {
        let event = match kind {
            EntryKind::SessionStarted => SessionEvent::SessionStarted {
                targets: targets.get(&seq).cloned().unwrap_or_default(),
                capabilities: session_capabilities.clone(),
            },
            EntryKind::TargetsUpserted => SessionEvent::TargetsUpserted {
                targets: targets.get(&seq).cloned().unwrap_or_default(),
            },
            EntryKind::TodosReplaced => SessionEvent::TodosReplaced {
                items: todos.get(&seq).cloned().unwrap_or_default(),
            },
            EntryKind::AgentCompleted => SessionEvent::AgentCompleted,
            EntryKind::AgentInterrupted => SessionEvent::AgentInterrupted,
            _ => events
                .remove(&seq)
                .ok_or_else(|| corrupt(format!("entry {seq} ({kind}) has no {kind} row")))?,
        };
        records.push(EventRecord {
            id: event_id(public_id)?,
            sequence: self::sequence(seq),
            timestamp_millis: created,
            agent: agent_of(agent)?,
            event,
        });
    }
    Ok(records)
}

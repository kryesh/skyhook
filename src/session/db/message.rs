//! Blobs and messages: committed conversation content and the runtime parts within it.

use std::collections::HashMap;

use super::{decode::*, encode::*, *};
use crate::{
    agent::TodoItem,
    execution::{ExecutionLocation, path_bytes, path_from_bytes},
    job::{AgentMessage, AgentProgress},
    media::{AttachmentRef, BlobDigest, BlobRef, ImageFormat, ImageRef, TextRef},
    named_enum::named_enum,
    provider::protocol::{
        AssistantItem, ItemId, ItemKind, Position, Provenance, Replay, TextBlock, ToolCall,
        ToolResult,
    },
    session::{
        EntryKind, JobEvent, Message, RuntimeState, SessionEvent, StateJob, StateJobKind, UserPart,
    },
};

named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
    pub(super) enum MessageRole {
        User = "user",
        Assistant = "assistant",
        Tool = "tool",
    }
}

named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
    pub(super) enum UserPartKind {
        Text = "text",
        Attachment = "attachment",
        State = "state",
        JobEvents = "job_events",
        ParentInput = "parent_input",
        Compaction = "compaction",
    }
}

impl UserPartKind {
    /// Whether the part holds its text inline.
    pub(super) fn text(self) -> bool {
        matches!(self, Self::Text | Self::ParentInput | Self::Compaction)
    }
}

named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
    pub(super) enum JobEventKind {
        Message = "message",
        Job = "job",
    }
}

impl Encoder {
    /// Insert a message committed by `agent`; a tool result binds to that agent's
    /// latest committed call of the same id that has no result yet.
    pub(super) fn message_for(&mut self, db: &Db, agent: i64, message: &Message) -> DbResult<i64> {
        let Message::Tool(results) = message else {
            return self.message(db, message);
        };
        let [result] = results.as_slice() else {
            return Err(rejected("a tool message carries exactly one result"));
        };
        let (call, name) = db
            .query_row(
                "SELECT c.item, c.name FROM tool_call c \
                 JOIN assistant_item i ON i.id = c.item \
                 JOIN message_commit m ON m.message = i.message \
                 JOIN entry e ON e.seq = m.entry \
                 WHERE e.agent = ?1 AND c.call_id = ?2 \
                 AND NOT EXISTS (SELECT 1 FROM tool_result r WHERE r.call = c.item) \
                 ORDER BY m.entry DESC LIMIT 1",
                params![agent, &result.call_id],
                |row| Ok((row.get::<i64>(0)?, row.get::<String>(1)?)),
            )?
            .ok_or_else(|| rejected("tool result answers no open committed call"))?;
        if name != result.name {
            return Err(rejected("tool result name differs from its call"));
        }
        let message = db.insert(
            "INSERT INTO message (role) VALUES (?1)",
            params![MessageRole::Tool],
        )?;
        tool_result(db, message, call, result)?;
        Ok(message)
    }

    pub(super) fn message(&mut self, db: &Db, message: &Message) -> DbResult<i64> {
        match message {
            Message::User(parts) => {
                let id = db.insert(
                    "INSERT INTO message (role) VALUES (?1)",
                    params![MessageRole::User],
                )?;
                for (position, part) in parts.iter().enumerate() {
                    let (kind, text, attachment) = match part {
                        UserPart::Text { text } => (UserPartKind::Text, Some(text), None),
                        UserPart::ParentInput { text } => {
                            (UserPartKind::ParentInput, Some(text), None)
                        }
                        UserPart::Compaction { text } => {
                            (UserPartKind::Compaction, Some(text), None)
                        }
                        UserPart::Attachment { attachment } => {
                            (UserPartKind::Attachment, None, Some(attachment))
                        }
                        UserPart::State { .. } => (UserPartKind::State, None, None),
                        UserPart::JobEvents { .. } => (UserPartKind::JobEvents, None, None),
                    };
                    let (blob, format, file) = match attachment {
                        Some(AttachmentRef::Text(text)) => {
                            (Some(&text.blob), None, text.file.clone())
                        }
                        Some(AttachmentRef::Image(image)) => {
                            (Some(&image.blob), Some(image.format), image.file.clone())
                        }
                        None => (None, None, None),
                    };
                    let blob = blob.map(|blob| stored_blob(db, blob)).transpose()?;
                    let row = db.insert(
                        "INSERT INTO user_part (message, position, kind, text, blob, \
                         image_format, file) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![id, position, kind, text, blob, format, file],
                    )?;
                    match part {
                        UserPart::State { state } => self.state(db, row, state)?,
                        UserPart::JobEvents { events } => job_events(db, row, events)?,
                        _ => {}
                    }
                }
                Ok(id)
            }
            Message::Assistant(items) => {
                let id = db.insert(
                    "INSERT INTO message (role) VALUES (?1)",
                    params![MessageRole::Assistant],
                )?;
                for item in items {
                    assistant_item(db, id, item)?;
                }
                Ok(id)
            }
            Message::Tool(_) => Err(rejected(
                "tool results are only stored as committed agent messages",
            )),
        }
    }

    fn state(&mut self, db: &Db, part: i64, state: &RuntimeState) -> DbResult<()> {
        let target = self.target(db, &state.location.target)?;
        db.execute(
            "INSERT INTO user_part_state (part, date, location_target, location_workspace) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                part,
                &state.date,
                target,
                path_bytes(&state.location.workspace)
            ],
        )?;
        self.state_jobs(db, part, &state.jobs, None, &mut 0)?;
        for (position, item) in state.todos.iter().enumerate() {
            db.execute(
                "INSERT INTO user_part_state_todo (part, position, text, status) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![part, position, &item.text, item.status],
            )?;
        }
        Ok(())
    }

    /// Jobs in pre-order, each child naming its parent's position.
    fn state_jobs(
        &mut self,
        db: &Db,
        part: i64,
        jobs: &[StateJob],
        parent: Option<u64>,
        next: &mut u64,
    ) -> DbResult<()> {
        for job in jobs {
            let position = *next;
            *next += 1;
            let target = job
                .target
                .as_ref()
                .map(|target| self.target(db, target))
                .transpose()?;
            let (tool, progress) = match &job.kind {
                StateJobKind::Agent { progress } => (None, Some(*progress)),
                StateJobKind::Tool { tool } => (Some(tool), None),
            };
            db.execute(
                "INSERT INTO user_part_state_job (part, position, parent_position, job, tool, \
                 name, state, target, workspace, age_seconds, turns, tool_calls) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    part,
                    position,
                    parent,
                    job.job.get(),
                    tool,
                    job.name.as_ref(),
                    job.state,
                    target,
                    path_bytes(&job.workspace),
                    job.age_seconds,
                    progress.map(|progress| progress.turns),
                    progress.map(|progress| progress.tool_calls),
                ],
            )?;
            self.state_jobs(db, part, &job.children, Some(position), next)?;
        }
        Ok(())
    }
}

fn job_events(db: &Db, part: i64, events: &[JobEvent]) -> DbResult<()> {
    for (position, event) in events.iter().enumerate() {
        match event {
            JobEvent::Message(message) => {
                db.execute(
                    "INSERT INTO user_part_job_event (part, position, kind, job, name, \
                     source, text) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        part,
                        position,
                        JobEventKind::Message,
                        message.id.get(),
                        message.name.as_ref(),
                        message.message,
                        &message.text
                    ],
                )?;
            }
            JobEvent::Job(view) => {
                let job = view
                    .id
                    .ok_or_else(|| rejected("job event view names no job"))?;
                db.execute(
                    "INSERT INTO user_part_job_event (part, position, kind, job, view) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![part, position, JobEventKind::Job, job.get(), json(view)?],
                )?;
            }
        }
    }
    Ok(())
}

fn assistant_item(db: &Db, message: i64, item: &AssistantItem) -> DbResult<()> {
    let kind = item.kind();
    let id = db.insert(
        "INSERT INTO assistant_item (message, position, provider_id, kind) \
         VALUES (?1, ?2, ?3, ?4)",
        params![message, item.position().get(), item.id(), kind],
    )?;
    if let Some(replay) = item.replay() {
        db.execute(
            "INSERT INTO reasoning_replay (item, format, model, scope, payload, binding) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                replay.provenance.format,
                &replay.provenance.model,
                &replay.provenance.scope,
                json(&replay.payload)?,
                replay.binding
            ],
        )?;
    }
    match item {
        AssistantItem::Text { blocks, .. } | AssistantItem::Reasoning { blocks, .. } => {
            for block in blocks {
                db.execute(
                    "INSERT INTO assistant_block (item, item_kind, position, provider_id, text) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        id,
                        kind,
                        block.position.get(),
                        &block.id,
                        block.text.clone()
                    ],
                )?;
            }
        }
        AssistantItem::ToolCall { call, .. } => {
            db.execute(
                "INSERT INTO tool_call (item, call_id, name, arguments) VALUES (?1, ?2, ?3, ?4)",
                params![id, call.id(), call.name(), json(call.arguments())?],
            )?;
        }
    }
    Ok(())
}

fn tool_result(db: &Db, message: i64, call: i64, result: &ToolResult) -> DbResult<()> {
    db.execute(
        "INSERT INTO tool_result (message, call, result, is_error) VALUES (?1, ?2, ?3, ?4)",
        params![message, call, json(&result.result)?, result.is_error],
    )?;
    for (position, image) in result.images.iter().enumerate() {
        image_row(
            db,
            "tool_result_image",
            "result",
            message as u64,
            position,
            image,
        )?;
    }
    Ok(())
}

pub(super) fn image_row(
    db: &Db,
    table: &str,
    owner: &str,
    id: u64,
    position: usize,
    image: &ImageRef,
) -> DbResult<()> {
    let blob = stored_blob(db, &image.blob)?;
    db.execute(
        &format!(
            "INSERT INTO {table} ({owner}, position, blob, format, file) VALUES (?1, ?2, ?3, ?4, ?5)"
        ),
        params![id, position, blob, image.format, image.file.clone()],
    )?;
    Ok(())
}

/// A reference must name a stored blob of exactly its recorded length.
fn stored_blob(db: &Db, blob: &BlobRef) -> DbResult<Value> {
    let key = blob.sha256.to_bytes().to_vec();
    let length = db
        .query_row(
            "SELECT length(bytes) FROM blob WHERE sha256 = ?1",
            params![key.clone()],
            |row| Ok(row.get::<u64>(0)?),
        )?
        .ok_or_else(|| rejected(format!("blob {} is not stored", blob.sha256)))?;
    if length != blob.bytes {
        return Err(rejected(format!("blob {} length differs", blob.sha256)));
    }
    Ok(Value::Blob(key))
}

pub(super) fn todos(db: &Db, seq: u64, kind: EntryKind, items: &[TodoItem]) -> DbResult<()> {
    for (position, item) in items.iter().enumerate() {
        db.execute(
            "INSERT INTO todo_item (entry, kind, position, text, status) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![seq, kind, position, &item.text, item.status],
        )?;
    }
    Ok(())
}

/// Every todo list an entry journaled.
pub(super) fn entry_todos(db: &Db) -> DbResult<HashMap<i64, Vec<TodoItem>>> {
    grouped(
        db,
        "SELECT entry, text, status FROM todo_item ORDER BY entry, position",
        todo,
    )
}

fn todo(row: &Row) -> DbResult<TodoItem> {
    Ok(TodoItem {
        text: parsed(row.get(1)?)?,
        status: enum_column(row, 2)?,
    })
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

/// Every message's rows, reassembled on demand.
pub(super) struct Messages {
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
        sha256: BlobDigest::from_bytes(super::decode::bytes(sha256)?),
        bytes,
    })
}

/// An image slot selected as (blob, length(blob bytes), format, file) from column `at`.
pub(super) fn image(row: &Row, at: i32) -> DbResult<ImageRef> {
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

/// The jobs under `parent` from pre-order `rows`, each followed by its descendants.
fn state_tree<'a>(
    rows: &mut std::iter::Peekable<impl Iterator<Item = &'a StateJobRow>>,
    parent: Option<u64>,
) -> Vec<StateJob> {
    let mut jobs = Vec::new();
    while let Some((position, _, job)) = rows.next_if(|(_, above, _)| *above == parent) {
        let children = state_tree(rows, Some(*position));
        jobs.push(StateJob {
            children,
            ..job.clone()
        });
    }
    jobs
}

fn state_job(row: &Row) -> DbResult<StateJobRow> {
    let progress = (row.get::<Option<u64>>(10)?, row.get::<Option<u64>>(11)?);
    let kind = match (row.get::<Option<String>>(4)?, progress) {
        (None, (Some(turns), Some(tool_calls))) => StateJobKind::Agent {
            progress: AgentProgress { turns, tool_calls },
        },
        (Some(tool), (None, None)) => StateJobKind::Tool { tool },
        _ => return Err(corrupt("state job progress does not match its tool")),
    };
    Ok((
        row.get::<u64>(1)?,
        row.get::<Option<u64>>(2)?,
        StateJob {
            job: job(row.get(3)?)?,
            kind,
            name: row.get::<Option<String>>(5)?.map(parsed).transpose()?,
            state: enum_column(row, 6)?,
            target: row.get::<Option<String>>(7)?.map(parsed).transpose()?,
            workspace: path_from_bytes(row.get(8)?),
            age_seconds: row.get(9)?,
            children: Vec::new(),
        },
    ))
}

fn job_event(row: &Row) -> DbResult<JobEvent> {
    Ok(match enum_column(row, 1)? {
        JobEventKind::Message => JobEvent::Message(AgentMessage {
            id: job(row.get(2)?)?,
            name: row.get::<Option<String>>(3)?.map(parsed).transpose()?,
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
    pub(super) fn load(db: &Db) -> DbResult<Self> {
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
                |row| Ok((row.get(1)?, location(row.get(2)?, row.get(3)?)?)),
            )?,
            state_jobs: grouped(
                db,
                "SELECT j.part, j.position, j.parent_position, j.job, j.tool, j.name, j.state, \
                 t.name, j.workspace, j.age_seconds, j.turns, j.tool_calls \
                 FROM user_part_state_job j LEFT JOIN target t ON t.id = j.target \
                 ORDER BY j.part, j.position",
                state_job,
            )?,
            state_todos: grouped(
                db,
                "SELECT part, text, status FROM user_part_state_todo ORDER BY part, position",
                todo,
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
                "SELECT item, format, model, scope, payload, binding FROM reasoning_replay",
                |row| {
                    Ok(Replay {
                        provenance: Provenance {
                            format: enum_column(row, 1)?,
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

    pub(super) fn message(&self, id: i64) -> DbResult<Message> {
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
                            let mut rows =
                                self.state_jobs.get(part).into_iter().flatten().peekable();
                            let jobs = state_tree(&mut rows, None);
                            if rows.next().is_some() {
                                return Err(corrupt("state jobs are not in pre-order"));
                            }
                            UserPart::State {
                                state: RuntimeState {
                                    date,
                                    jobs,
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

    /// Every committed message.
    pub(super) fn events(&self, events: &mut Events) -> DbResult<()> {
        events.load("SELECT entry, message FROM message_commit", |row| {
            Ok(SessionEvent::MessageCommitted {
                message: self.message(row.get(1)?)?,
            })
        })
    }
}

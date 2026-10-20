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
    session::{JobEvent, Message, RuntimeState, SessionEvent, StateJob, StateJobKind, UserPart},
};

named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
    pub(super) enum MessageRole {
        User = "user",
        Assistant = "assistant",
        Tool = "tool",
        State = "state",
    }
}

named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
    pub(super) enum UserPartKind {
        Text = "text",
        Attachment = "attachment",
        JobEvents = "job_events",
        ParentInput = "parent_input",
        Compaction = "compaction",
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
        let Message::Tool(result) = message else {
            return self.message(db, message);
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
        let message = message_row(db, MessageRole::Tool)?;
        tool_result(db, message, call, result)?;
        Ok(message)
    }

    pub(super) fn message(&mut self, db: &Db, message: &Message) -> DbResult<i64> {
        let role = match message {
            Message::User(_) => MessageRole::User,
            Message::Assistant(_) => MessageRole::Assistant,
            Message::State(_) => MessageRole::State,
            Message::Tool(_) => {
                return Err(rejected(
                    "tool results are only stored as committed agent messages",
                ));
            }
        };
        let id = message_row(db, role)?;
        match message {
            Message::User(parts) => {
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
                    let blob = blob.map(|blob| blob.sha256.to_bytes().to_vec());
                    let row = db.insert(
                        "INSERT INTO user_part (message, position, kind, text, blob, \
                         image_format, file) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![id, position, kind, text, blob, format, file],
                    )?;
                    if let UserPart::JobEvents { events } = part {
                        job_events(db, row, events)?;
                    }
                }
            }
            Message::Assistant(items) => {
                for item in items {
                    assistant_item(db, id, item)?;
                }
            }
            Message::State(state) => self.state(db, id, state)?,
            Message::Tool(_) => {}
        }
        Ok(id)
    }

    fn state(&mut self, db: &Db, message: i64, state: &RuntimeState) -> DbResult<()> {
        let target = self.target(db, &state.location.target)?;
        db.execute(
            "INSERT INTO message_state (message, date, location_target, location_workspace) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                message,
                &state.date,
                target,
                path_bytes(&state.location.workspace)
            ],
        )?;
        self.state_jobs(db, message, &state.jobs, None, &mut 0)?;
        todos(
            db,
            "message_state_todo",
            "message",
            message as u64,
            &state.todos,
        )
    }

    /// Jobs in pre-order, each child naming its parent's position.
    fn state_jobs(
        &mut self,
        db: &Db,
        message: i64,
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
                "INSERT INTO message_state_job (message, position, parent_position, job, tool, \
                 name, state, target, workspace, age_seconds, turns, tool_calls) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    message,
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
            self.state_jobs(db, message, &job.children, Some(position), next)?;
        }
        Ok(())
    }
}

fn message_row(db: &Db, role: MessageRole) -> DbResult<i64> {
    db.insert("INSERT INTO message (role) VALUES (?1)", params![role])
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
    let id = db.insert(
        "INSERT INTO assistant_item (message, position, provider_id, kind) \
         VALUES (?1, ?2, ?3, ?4)",
        params![message, item.position().get(), item.id(), item.kind()],
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
                    "INSERT INTO assistant_block (item, position, provider_id, text) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![id, block.position.get(), &block.id, block.text.clone()],
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
    let blob = image.blob.sha256.to_bytes().to_vec();
    db.execute(
        format!(
            "INSERT INTO {table} ({owner}, position, blob, format, file) VALUES (?1, ?2, ?3, ?4, ?5)"
        ),
        params![id, position, blob, image.format, image.file.clone()],
    )?;
    Ok(())
}

/// Insert `items` as the `table` rows of `owner` `id`.
pub(super) fn todos(
    db: &Db,
    table: &str,
    owner: &str,
    id: u64,
    items: &[TodoItem],
) -> DbResult<()> {
    for (position, item) in items.iter().enumerate() {
        db.execute(
            format!(
                "INSERT INTO {table} ({owner}, position, text, status) VALUES (?1, ?2, ?3, ?4)"
            ),
            params![id, position, &item.text, item.status],
        )?;
    }
    Ok(())
}

/// Every todo list an entry in scope journaled.
pub(super) fn entry_todos(events: &Events) -> DbResult<HashMap<i64, Vec<TodoItem>>> {
    grouped(
        events.db,
        events.scoped(
            "SELECT entry, text, status FROM todo_item {scope} ORDER BY entry, position",
            "entry",
        )?,
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
    /// date, location, per state message
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

/// Whether every row of `rows` belongs to an owner `owners` holds as one of `kinds`.
fn filed<T, K: PartialEq>(rows: &HashMap<i64, T>, owners: &HashMap<i64, K>, kinds: &[K]) -> bool {
    (rows.keys()).all(|owner| owners.get(owner).is_some_and(|kind| kinds.contains(kind)))
}

/// The messages committed at `commits`, by commit.
pub(in crate::session) fn committed(
    db: &Db,
    commits: &[MessageSeq],
) -> DbResult<HashMap<MessageSeq, Message>> {
    let commits: Vec<_> = commits.iter().map(|commit| commit.get()).collect();
    let rows = db.query(
        "SELECT entry, message FROM message_commit \
         WHERE entry IN (SELECT value FROM json_each(?1))",
        params![json(&commits)?],
        |row| Ok((sequence(row.get(0)?).message(), row.get::<i64>(1)?)),
    )?;
    Ok(load_committed(db, rows)?.into_iter().collect())
}

/// The message of each `(commit, message row)` pair, in order.
fn load_committed(db: &Db, rows: Vec<(MessageSeq, i64)>) -> DbResult<Vec<(MessageSeq, Message)>> {
    let ids: Vec<_> = rows.iter().map(|(_, id)| *id).collect();
    let messages = Messages::load(db, Scope::Only(&ids))?;
    (rows.into_iter())
        .map(|(commit, id)| Ok((commit, messages.message(id)?)))
        .collect()
}

/// The rows a load reassembles: every one, or those of the listed keys.
#[derive(Clone, Copy)]
pub(in crate::session) enum Scope<'a, K = i64> {
    All,
    Only(&'a [K]),
}

/// What ties a table's rows to their key: a key column, or the assistant item or user
/// part the row belongs to.
#[derive(Clone, Copy)]
pub(super) enum Owner {
    Message(&'static str),
    Item,
    Part,
}

impl<K: serde::Serialize> Scope<'_, K> {
    /// `sql` with `{scope}` replaced by a filter that keeps the rows `owner` ties
    /// to a key in scope.
    pub(super) fn query(self, sql: &str, owner: Owner) -> DbResult<Query> {
        const SCOPED: &str = "IN (SELECT value FROM json_each(?1))";
        Ok(match self {
            Self::All => Query {
                sql: sql.replace("{scope}", ""),
                params: Vec::new(),
            },
            Self::Only(messages) => {
                let filter = match owner {
                    Owner::Message(column) => format!("{column} {SCOPED}"),
                    Owner::Item => {
                        format!("item IN (SELECT id FROM assistant_item WHERE message {SCOPED})")
                    }
                    Owner::Part => {
                        format!("e.part IN (SELECT id FROM user_part WHERE message {SCOPED})")
                    }
                };
                Query {
                    sql: sql.replace("{scope}", &format!("WHERE {filter}")),
                    params: params![json(&messages)?],
                }
            }
        })
    }
}

impl Messages {
    pub(super) fn load(db: &Db, scope: Scope) -> DbResult<Self> {
        let messages = Self {
            roles: keyed(
                db,
                scope.query("SELECT id, role FROM message {scope}", Owner::Message("id"))?,
                |row| enum_column(row, 1),
            )?,
            parts: grouped(
                db,
                scope.query(
                    "SELECT p.message, p.id, p.kind, p.text, p.blob, length(b.bytes), \
                     p.image_format, p.file FROM user_part p LEFT JOIN blob b ON b.sha256 = p.blob \
                     {scope} ORDER BY p.message, p.position",
                    Owner::Message("p.message"),
                )?,
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
                scope.query(
                    "SELECT s.message, s.date, t.name, s.location_workspace FROM message_state s \
                     JOIN target t ON t.id = s.location_target {scope}",
                    Owner::Message("s.message"),
                )?,
                |row| Ok((row.get(1)?, location(row.get(2)?, row.get(3)?)?)),
            )?,
            state_jobs: grouped(
                db,
                scope.query(
                    "SELECT j.message, j.position, j.parent_position, j.job, j.tool, j.name, \
                     j.state, t.name, j.workspace, j.age_seconds, j.turns, j.tool_calls \
                     FROM message_state_job j LEFT JOIN target t ON t.id = j.target \
                     {scope} ORDER BY j.message, j.position",
                    Owner::Message("j.message"),
                )?,
                state_job,
            )?,
            state_todos: grouped(
                db,
                scope.query(
                    "SELECT message, text, status FROM message_state_todo \
                     {scope} ORDER BY message, position",
                    Owner::Message("message"),
                )?,
                todo,
            )?,
            job_events: grouped(
                db,
                scope.query(
                    "SELECT e.part, e.kind, e.job, e.name, e.source, e.text, e.view \
                     FROM user_part_job_event e {scope} ORDER BY e.part, e.position",
                    Owner::Part,
                )?,
                job_event,
            )?,
            items: grouped(
                db,
                scope.query(
                    "SELECT message, id, position, provider_id, kind FROM assistant_item \
                     {scope} ORDER BY message, position",
                    Owner::Message("message"),
                )?,
                |row| Ok((row.get(1)?, row.get(2)?, row.get(3)?, enum_column(row, 4)?)),
            )?,
            replays: keyed(
                db,
                scope.query(
                    "SELECT item, format, model, scope, payload, binding FROM reasoning_replay \
                     {scope}",
                    Owner::Item,
                )?,
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
                scope.query(
                    "SELECT item, position, provider_id, text FROM assistant_block \
                     {scope} ORDER BY item, position",
                    Owner::Item,
                )?,
                |row| Ok((row.get(1)?, row.get(2)?, row.get(3)?)),
            )?,
            calls: keyed(
                db,
                scope.query(
                    "SELECT item, call_id, name, arguments FROM tool_call {scope}",
                    Owner::Item,
                )?,
                |row| Ok((row.get(1)?, row.get(2)?, row.get(3)?)),
            )?,
            results: keyed(
                db,
                scope.query(
                    "SELECT r.message, c.call_id, c.name, r.result, r.is_error FROM tool_result r \
                     JOIN tool_call c ON c.item = r.call {scope}",
                    Owner::Message("r.message"),
                )?,
                |row| Ok((row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )?,
            images: grouped(
                db,
                scope.query(
                    "SELECT i.result, i.blob, length(b.bytes), i.format, i.file \
                     FROM tool_result_image i JOIN blob b ON b.sha256 = i.blob \
                     {scope} ORDER BY i.result, i.position",
                    Owner::Message("i.result"),
                )?,
                |row| image(row, 1),
            )?,
        };
        let items: HashMap<_, _> = (messages.items.values().flatten())
            .map(|(item, _, _, kind)| (*item, *kind))
            .collect();
        let roles = &messages.roles;
        if !(filed(&messages.parts, roles, &[MessageRole::User])
            && filed(&messages.states, roles, &[MessageRole::State])
            && filed(&messages.items, roles, &[MessageRole::Assistant])
            && filed(&messages.results, roles, &[MessageRole::Tool])
            && filed(
                &messages.blocks,
                &items,
                &[ItemKind::Text, ItemKind::Reasoning],
            )
            && filed(&messages.replays, &items, &[ItemKind::Reasoning]))
        {
            return Err(corrupt("message rows belong to another role or item kind"));
        }
        Ok(messages)
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
            MessageRole::State => {
                let (date, location) = self
                    .states
                    .get(&id)
                    .cloned()
                    .ok_or_else(|| corrupt("state message has no state"))?;
                let mut rows = self.state_jobs.get(&id).into_iter().flatten().peekable();
                let jobs = state_tree(&mut rows, None);
                if rows.next().is_some() {
                    return Err(corrupt("state jobs are not in pre-order"));
                }
                Message::State(RuntimeState {
                    date,
                    jobs,
                    todos: self.state_todos.get(&id).cloned().unwrap_or_default(),
                    location,
                })
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
                Message::Tool(ToolResult {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    result: parse_json(result)?,
                    images: self.images.get(&id).cloned().unwrap_or_default(),
                    is_error: *is_error,
                })
            }
        })
    }

    /// The commits in scope.
    pub(super) fn events(&self, events: &mut Events) -> DbResult<()> {
        let sql = "SELECT entry, message FROM message_commit {scope}";
        events.load(sql, "entry", |row| {
            Ok(SessionEvent::MessageCommitted {
                message: self.message(row.get(1)?)?,
            })
        })
    }
}

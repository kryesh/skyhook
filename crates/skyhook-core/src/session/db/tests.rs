//! Encode → decode round trips and schema-level rejection of inconsistent rows.

use serde_json::json;

use crate::{
    execution::ExecutionLocation,
    identity::{AgentId, EventId, JobId, QueueAttemptId, SessionId},
    job::{JobRole, JobState},
    media::{AttachmentRef, BlobRef, ImageFormat, ImageRef, TextRef},
    provider::protocol::{
        AssistantItem, HistoryLifetime, Message, ReplayEnvelope, ResponseSchema, StopReason,
        SystemSegment, ToolCall, ToolDefinition, ToolResult, Usage, UserContent,
    },
    session::{
        CompactionCheckpoint, EventRecord, ModelCallOrigin, ModelContext, ModelFailureKind,
        ModelPurpose, QueueIntent, QueueSettlement, SessionEvent,
        fixture::{child_started, profile, start_events},
    },
};

use super::{Db, Encoder, OpenMode, decode_records};

struct Fixture {
    db: Db,
    encoder: Encoder,
    records: Vec<EventRecord>,
    session: SessionId,
}

impl Fixture {
    fn new() -> Self {
        Self {
            db: Db::open(std::path::Path::new(""), OpenMode::Memory).unwrap(),
            encoder: Encoder::default(),
            records: Vec::new(),
            session: SessionId::from_bytes([3; 16]),
        }
    }

    fn root(&self) -> AgentId {
        AgentId::root(self.session)
    }

    fn blob(&self, bytes: &[u8]) -> BlobRef {
        let blob = BlobRef::of(bytes);
        self.db
            .execute(
                "INSERT INTO blob (sha256, bytes) VALUES (?1, ?2)",
                params![blob.sha256.to_bytes().to_vec(), bytes.to_vec()],
            )
            .unwrap();
        blob
    }

    /// Commit events as one transaction; returns their sequences.
    fn commit(
        &mut self,
        events: Vec<(AgentId, Option<QueueAttemptId>, SessionEvent)>,
    ) -> Result<Vec<u64>, super::DbError> {
        let first = self.records.len() as u64 + 1;
        let records: Vec<_> = events
            .into_iter()
            .enumerate()
            .map(|(offset, (agent, attempt, event))| EventRecord {
                id: EventId::generate().unwrap(),
                queue_attempt: attempt.or(event.queue_attempt()),
                sequence: first + offset as u64,
                timestamp_millis: 1_700_000_000_000 + offset as i64,
                agent,
                event,
            })
            .collect();
        let (db, encoder) = (&self.db, &mut self.encoder);
        let result = db.transaction(|| {
            let tx = encoder.begin_tx(db, 0)?;
            encoder.records(db, tx, &records)
        });
        match result {
            Ok(()) => db.commit().unwrap(),
            Err(error) => {
                encoder.reset();
                return Err(error);
            }
        }
        let sequences = records.iter().map(|record| record.sequence).collect();
        self.records.extend(records);
        Ok(sequences)
    }

    /// Start the session and its root agent.
    fn start(&mut self, workspace: &str) -> AgentId {
        let root = self.root();
        let events = start_events(&root, std::path::Path::new(workspace));
        let events = events
            .into_iter()
            .map(|(agent, event)| (agent, None, event));
        self.commit(events.collect()).unwrap();
        root
    }

    fn one(&mut self, agent: AgentId, event: SessionEvent) -> u64 {
        self.commit(vec![(agent, None, event)]).unwrap()[0]
    }

    fn reject(&mut self, agent: AgentId, event: SessionEvent) {
        let before = self.records.len();
        assert!(
            self.commit(vec![(agent, None, event.clone())]).is_err(),
            "accepted {event:?}"
        );
        assert_eq!(self.records.len(), before);
    }

    fn assert_round_trip(&self) {
        let decoded = decode_records(&self.db, self.session).unwrap();
        assert_eq!(decoded.len(), self.records.len());
        for (decoded, expected) in decoded.iter().zip(&self.records) {
            assert_eq!(decoded, expected);
        }
    }
}

fn user(text: &str) -> Message {
    Message::User(vec![UserContent::Text { text: text.into() }])
}

fn call(id: &str, name: &str) -> AssistantItem {
    AssistantItem::tool_call(
        format!("item-{id}"),
        0,
        ToolCall::new(id, name, json!({"path": "file"})).unwrap(),
    )
}

fn result(id: &str, name: &str, images: Vec<ImageRef>) -> SessionEvent {
    SessionEvent::MessageCommitted {
        message: Message::Tool(vec![ToolResult {
            call_id: id.into(),
            name: name.into(),
            result: json!({"ok": true, "nested": [1, null]}),
            images,
            is_error: false,
        }]),
    }
}

#[test]
fn every_event_kind_round_trips() {
    let mut fixture = Fixture::new();
    let root = fixture.start("/workspace");
    macro_rules! one {
        ($event:expr $(,)?) => {
            fixture.one(root.clone(), $event)
        };
    }
    let image = fixture.blob(crate::tests::png(b"image").bytes());
    let notes = fixture.blob(b"notes");
    let png = ImageRef {
        file: Some("image.png".into()),
        format: ImageFormat::Png,
        blob: image,
    };
    let agent_context = ModelContext {
        purpose: ModelPurpose::Agent,
        profile: profile(),
        system: vec![SystemSegment {
            text: "system".into(),
            cache: true,
        }],
        tools: vec![ToolDefinition {
            name: "read".into(),
            description: "read a file".into(),
            input_schema: json!({"type": "object"}),
        }],
        response_schema: None,
    };
    let context = one!(SessionEvent::ModelContext {
        context: agent_context.clone(),
    });
    let prompt = one!(SessionEvent::MessageCommitted {
        message: Message::User(vec![
            UserContent::Text {
                text: "look".into(),
            },
            UserContent::Attachment {
                attachment: AttachmentRef::Image(png.clone()),
            },
            UserContent::Attachment {
                attachment: AttachmentRef::Text(TextRef {
                    file: None,
                    blob: notes,
                }),
            },
            UserContent::Runtime {
                text: "state".into(),
            },
            UserContent::ParentInput {
                text: "parent".into(),
            },
        ]),
    });
    let request = one!(SessionEvent::ModelRequested {
        context,
        history: vec![prompt],
        tail: vec![user("tail")],
        history_lifetime: HistoryLifetime::Ending,
        purpose: ModelPurpose::Agent,
    });
    one!(SessionEvent::ModelAttemptStarted {
        request,
        attempt: 1
    });
    one!(SessionEvent::ModelFailed {
        request,
        attempt: 1,
        error: "lost".into(),
        kind: ModelFailureKind::Error,
    });
    one!(SessionEvent::ModelRecoveryScheduled {
        request,
        attempt: 2,
        max_attempts: Some(3),
        delay_millis: 1000,
        error: "lost".into(),
    });
    one!(SessionEvent::ModelAttemptStarted {
        request,
        attempt: 2
    });
    let replay = ReplayEnvelope {
        version: 1,
        protocol: "responses".into(),
        model: "model".into(),
        scope: "reasoning".into(),
        payload: json!({"encrypted": "opaque"}),
        conversation_bound: true,
    };
    let assistant = one!(SessionEvent::MessageCommitted {
        message: Message::Assistant(vec![
            AssistantItem::reasoning("reason", 0, "thinking", Some(replay)),
            AssistantItem::text("answer", 1, "text"),
            AssistantItem::tool_call(
                "call-a",
                2,
                ToolCall::new("a", "read", json!({"path": "a"})).unwrap(),
            ),
            AssistantItem::tool_call(
                "call-b",
                3,
                ToolCall::new("b", "read", json!({"path": "b"})).unwrap(),
            ),
        ]),
    });
    one!(SessionEvent::Usage {
        request: Some(request),
        usage: Usage {
            input_tokens: 10,
            cached_input_tokens: 2,
            output_tokens: 3,
        },
    });
    one!(SessionEvent::ResponseCompleted {
        request,
        attempt: 2,
        message: Some(assistant),
        stop_reason: StopReason::Other("custom".into()),
    });
    let job = JobId::new(1).unwrap();
    one!(SessionEvent::JobCreated {
        job,
        parent: None,
        origin: Some(ModelCallOrigin {
            message: assistant,
            call_id: "b".into(),
        }),
        tool: "read".into(),
        role: JobRole::Tool,
        name: Some("reader".into()),
        arguments: json!({"path": "b"}),
        output_schema: Some(json!({"type": "object"})),
        accepts_input: false,
        background: true,
        authorization_scope: Some(7),
        location: ExecutionLocation::named("build", "/srv".into()),
    });
    one!(SessionEvent::JobStateChanged {
        job,
        state: JobState::Running,
    });
    one!(result("b", "read", vec![png.clone()]));
    one!(result("a", "read", Vec::new()));
    one!(SessionEvent::JobFinished {
        job,
        state: JobState::Interrupted,
        error: Some("stopped".into()),
        images: vec![png.clone()],
        denial: Some(crate::tool::Denial::permission_denied()),
    });
    one!(SessionEvent::JobFinished {
        job,
        state: JobState::Cancelled,
        error: None,
        images: Vec::new(),
        denial: None,
    });
    let resource = crate::tool::policy::ResourceId::custom("plugin", ["server", "tool"]).unwrap();
    let grant = one!(SessionEvent::ApprovalGranted {
        job,
        grant: crate::tool::policy::ApprovalGrant::descendants(
            crate::tool::policy::Capability::Mcp,
            resource,
        ),
    });
    one!(SessionEvent::ApprovalRevoked { grant });
    one!(SessionEvent::JobClaimed { job });
    let notification = Some(prompt);
    one!(SessionEvent::JobInjected { job, notification });
    one!(SessionEvent::JobMessageDelivered {
        job,
        source: assistant,
        notification: prompt,
    });
    one!(SessionEvent::QuestionOpened {
        job,
        question_id: "q".into(),
        questions: json!([{"question": "why?"}]),
    });
    one!(SessionEvent::QuestionResolved {
        job,
        question_id: "q".into(),
        answers: json!(["because"]),
    });
    let attempt = QueueAttemptId::from_bytes([9; 16]);
    let intent = QueueIntent {
        attempt,
        content: vec![UserContent::Text {
            text: "queued".into(),
        }],
        model: Some("test".into()),
    };
    one!(SessionEvent::QueueIntent {
        intent: intent.clone(),
    });
    let message = Message::User(intent.content.clone());
    let bound = [
        SessionEvent::ModelChanged { profile: profile() },
        SessionEvent::MessageCommitted { message },
    ];
    let bound = bound.map(|event| (root.clone(), Some(attempt), event));
    fixture.commit(bound.into()).unwrap();
    let event = fixture.records.last().unwrap().id;
    one!(SessionEvent::QueueSettlement {
        attempt,
        settlement: QueueSettlement::Committed { event },
    });
    one!(SessionEvent::QueueAcknowledged { attempt });
    one!(SessionEvent::TodosReplaced {
        items: vec![crate::agent::TodoItem {
            text: "todo".into(),
            status: crate::agent::TodoStatus::InProgress,
        }],
    });
    one!(SessionEvent::TodosReplaced { items: Vec::new() });
    // A compaction summary request and its checkpoint.
    let frontier = fixture.records.len() as u64;
    let summary_context = one!(SessionEvent::ModelContext {
        context: ModelContext {
            purpose: ModelPurpose::Compaction,
            tools: Vec::new(),
            response_schema: Some(ResponseSchema {
                name: "summary".into(),
                schema: json!({"type": "object"}),
            }),
            ..agent_context
        },
    });
    let summary = one!(SessionEvent::ModelRequested {
        context: summary_context,
        history: vec![prompt, assistant],
        tail: vec![user("summarize")],
        history_lifetime: HistoryLifetime::Detached,
        purpose: ModelPurpose::Compaction,
    });
    one!(SessionEvent::ModelAttemptStarted {
        request: summary,
        attempt: 1,
    });
    let checkpoint = one!(SessionEvent::Compaction {
        checkpoint: CompactionCheckpoint {
            schema_version: 2,
            previous: None,
            frontier,
            message: Message::User(vec![UserContent::Compaction {
                text: "summary".into(),
            }]),
            todos: vec![crate::agent::TodoItem {
                text: "kept".into(),
                status: crate::agent::TodoStatus::Pending,
            }],
            retained: vec![prompt],
            request: summary,
            attempt: 1,
            max_context: 128_000,
            before_tokens: 100,
            after_tokens: 10,
        },
    });
    one!(SessionEvent::ModelRequested {
        context,
        history: vec![checkpoint, prompt],
        tail: Vec::new(),
        history_lifetime: HistoryLifetime::Continuing,
        purpose: ModelPurpose::Agent,
    });
    one!(SessionEvent::CompactionFailed {
        request: None,
        attempt: None,
        error: "no request".into(),
    });
    // A child agent with an owner job and its own events.
    let child = root.child(1);
    let owner = JobId::new(2).unwrap();
    one!(SessionEvent::JobCreated {
        job: owner,
        parent: None,
        origin: None,
        tool: "agent".into(),
        role: JobRole::Agent,
        name: None,
        arguments: json!({"prompt": "work"}),
        output_schema: None,
        accepts_input: true,
        background: false,
        authorization_scope: None,
        location: ExecutionLocation::root("/workspace".into()),
    });
    let location = ExecutionLocation::root("/workspace".into());
    let failed = SessionEvent::AgentFailed {
        error: "failed".into(),
    };
    let started = child_started(Some(root.clone()), Some(owner), location);
    let child_events = [started, failed].map(|event| (child.clone(), None, event));
    fixture.commit(child_events.into()).unwrap();
    for event in [
        SessionEvent::Status {
            message: "status".into(),
        },
        SessionEvent::TitleSet {
            title: "title".into(),
        },
        SessionEvent::SessionResumed,
        SessionEvent::AgentCompleted,
        SessionEvent::AgentInterrupted,
    ] {
        one!(event);
    }
    fixture.assert_round_trip();
}

#[test]
fn inconsistent_rows_are_rejected_without_partial_writes() {
    let mut fixture = Fixture::new();
    let root = fixture.root();
    let workspace = ExecutionLocation::root("/workspace".into());
    // Nothing references an agent before it starts.
    fixture.reject(root.clone(), SessionEvent::AgentCompleted);
    fixture.start("/workspace");
    macro_rules! one {
        ($event:expr $(,)?) => {
            fixture.one(root.clone(), $event)
        };
    }
    // A second root agent.
    fixture.reject(root.clone(), child_started(None, None, workspace));
    // Results need an open committed call; messages carry one result.
    fixture.reject(root.clone(), result("missing", "read", Vec::new()));
    let assistant = one!(SessionEvent::MessageCommitted {
        message: Message::Assistant(vec![call("a", "read")]),
    });
    fixture.reject(root.clone(), result("a", "write", Vec::new()));
    one!(result("a", "read", Vec::new()));
    fixture.reject(root.clone(), result("a", "read", Vec::new()));
    // Unknown jobs, terminal transitions and duplicate finishes.
    let job = JobId::new(1).unwrap();
    fixture.reject(
        root.clone(),
        SessionEvent::JobStateChanged {
            job,
            state: JobState::Running,
        },
    );
    one!(SessionEvent::JobCreated {
        job,
        parent: None,
        origin: Some(ModelCallOrigin {
            message: assistant,
            call_id: "a".into(),
        }),
        tool: "read".into(),
        role: JobRole::Tool,
        name: None,
        arguments: json!({}),
        output_schema: None,
        accepts_input: false,
        background: false,
        authorization_scope: None,
        location: ExecutionLocation::root("/workspace".into()),
    });
    fixture.reject(
        root.clone(),
        SessionEvent::JobStateChanged {
            job,
            state: JobState::Completed,
        },
    );
    let finished = |state| SessionEvent::JobFinished {
        job,
        state,
        error: None,
        images: Vec::new(),
        denial: None,
    };
    one!(finished(JobState::Completed));
    fixture.reject(root.clone(), finished(JobState::Failed));
    one!(SessionEvent::JobStateChanged {
        job,
        state: JobState::Running,
    });
    one!(finished(JobState::Failed));
    // Unstored blobs cannot be referenced.
    let image = ImageRef {
        file: None,
        format: ImageFormat::Png,
        blob: BlobRef::of(b"never stored"),
    };
    fixture.reject(
        root.clone(),
        SessionEvent::MessageCommitted {
            message: Message::User(vec![UserContent::Attachment {
                attachment: AttachmentRef::Image(image),
            }]),
        },
    );
    // Queue settlement must match the bound commit.
    let attempt = QueueAttemptId::from_bytes([1; 16]);
    one!(SessionEvent::QueueIntent {
        intent: QueueIntent {
            attempt,
            content: vec![UserContent::Text {
                text: "draft".into(),
            }],
            model: None,
        },
    });
    fixture.reject(
        root.clone(),
        SessionEvent::QueueSettlement {
            attempt,
            settlement: QueueSettlement::Committed {
                event: EventId::from_bytes([2; 16]),
            },
        },
    );
    fixture.reject(root.clone(), SessionEvent::QueueAcknowledged { attempt });
    one!(SessionEvent::QueueSettlement {
        attempt,
        settlement: QueueSettlement::NotCommitted,
    });
    let late = fixture.commit(vec![(
        root.clone(),
        Some(attempt),
        SessionEvent::MessageCommitted {
            message: user("draft"),
        },
    )]);
    assert!(late.is_err());
    // A failed batch leaves nothing behind, including rows of its valid events.
    let batch = fixture.commit(vec![
        (
            root.clone(),
            None,
            SessionEvent::Status {
                message: "kept?".into(),
            },
        ),
        (root.clone(), None, finished(JobState::Completed)),
    ]);
    assert!(batch.is_err());
    fixture.assert_round_trip();
    let foreign_keys = fixture
        .db
        .query("PRAGMA foreign_key_check", Vec::new(), |_| Ok(()))
        .unwrap();
    assert!(foreign_keys.is_empty());
}

#[test]
fn ledger_rows_are_append_only() {
    let mut fixture = Fixture::new();
    fixture.start("/w");
    for sql in [
        "UPDATE entry SET created_millis = 0",
        "DELETE FROM agent_capability",
        "UPDATE model_profile SET name = 'other'",
    ] {
        assert!(fixture.db.execute(sql, Vec::new()).is_err(), "{sql}");
    }
    fixture.assert_round_trip();
}

/// Only a run that restarts a finished job starts a new output generation; a
/// question answered mid-run, or a cancel after an interrupt, does not.
#[test]
fn job_generations_count_restarts_after_a_finish() {
    let mut fixture = Fixture::new();
    let root = fixture.start("/w");
    let job = JobId::new(1).unwrap();
    fixture.one(
        root.clone(),
        SessionEvent::JobCreated {
            job,
            parent: None,
            origin: None,
            tool: "agent".into(),
            role: JobRole::Agent,
            name: None,
            arguments: json!({}),
            output_schema: None,
            accepts_input: true,
            background: false,
            authorization_scope: None,
            location: ExecutionLocation::root("/w".into()),
        },
    );
    let finished = |state| SessionEvent::JobFinished {
        job,
        state,
        error: None,
        images: Vec::new(),
        denial: None,
    };
    let state = |state| SessionEvent::JobStateChanged { job, state };
    let generation = |fixture: &Fixture| {
        let sql = "SELECT generation FROM job_generation WHERE job = 1";
        let row = fixture
            .db
            .query_row(sql, Vec::new(), |row| Ok(row.get::<i64>(0)?));
        row.unwrap().unwrap()
    };
    for (event, expected) in [
        (state(JobState::Running), 0),
        (state(JobState::WaitingInput), 0),
        (state(JobState::Running), 0),
        (finished(JobState::Completed), 0),
        (state(JobState::Running), 1),
        (state(JobState::WaitingInput), 1),
        (state(JobState::Running), 1),
        (finished(JobState::Interrupted), 1),
        (finished(JobState::Cancelled), 1),
        (state(JobState::Running), 2),
    ] {
        fixture.one(root.clone(), event);
        assert_eq!(generation(&fixture), expected);
    }
}

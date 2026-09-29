//! Decompose session events into normalized rows inside the caller's transaction.
//! Each schema section's module writes the rows of its own events.
use std::collections::HashMap;

use serde::Serialize;

use super::{Db, DbResult, corrupt, params, rejected};
use crate::{
    identity::AgentId,
    session::{EntryKind, EventRecord, SessionEvent},
    target::TargetRef,
};

pub(super) fn json(value: &impl Serialize) -> DbResult<String> {
    serde_json::to_string(value).map_err(|error| corrupt(error.to_string()))
}

/// The entry an event's rows belong to.
#[derive(Clone, Copy)]
pub(super) struct Entry {
    pub seq: u64,
    pub kind: EntryKind,
    pub agent: i64,
}

/// Encodes records into one open transaction. Surrogate-key caches are only
/// valid for committed rows; the writer resets them after a rollback.
#[derive(Default)]
pub(in crate::session) struct Encoder {
    agents: HashMap<Vec<u32>, i64>,
    targets: HashMap<TargetRef, i64>,
}

impl Encoder {
    pub(in crate::session) fn reset(&mut self) {
        self.agents.clear();
        self.targets.clear();
    }

    /// Encode one transaction's records. Agents started by the batch get their rows
    /// first, since every entry, including a session start, references its agent.
    pub(in crate::session) fn records(&mut self, db: &Db, records: &[EventRecord]) -> DbResult<()> {
        for record in records {
            // The session's capability ceiling precedes the agent capabilities within it.
            if let SessionEvent::SessionStarted { capabilities, .. } = &record.event {
                db.execute(
                    "INSERT INTO session (singleton, public_id) VALUES (1, ?1)",
                    params![record.agent.session().to_bytes().to_vec()],
                )?;
                for capability in capabilities {
                    db.execute(
                        "INSERT INTO session_capability (capability) VALUES (?1)",
                        params![*capability],
                    )?;
                }
            }
            self.start_agent(db, record)?;
        }
        for record in records {
            let entry = Entry {
                seq: record.sequence.get(),
                kind: record.event.kind(),
                agent: self.agent(db, &record.agent)?,
            };
            db.execute(
                "INSERT INTO entry (seq, public_id, agent, created_millis, kind) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    entry.seq,
                    record.id.to_bytes().to_vec(),
                    entry.agent,
                    record.timestamp_millis,
                    entry.kind
                ],
            )
            .and_then(|_| self.subtype(db, entry, &record.event))
            .map_err(|error| match error {
                super::DbError::Sql(error) => {
                    rejected(format!("{} entry {}: {error}", entry.kind, record.sequence))
                }
                error => error,
            })?;
        }
        Ok(())
    }

    fn start_agent(&mut self, db: &Db, record: &EventRecord) -> DbResult<()> {
        if let SessionEvent::AgentStarted {
            owner_job,
            available_depth,
            ..
        } = &record.event
        {
            let parent = record
                .agent
                .parent()
                .map(|parent| self.agent(db, &parent))
                .transpose()?;
            let id = db.insert(
                "INSERT INTO agent (parent, child_index, owner_job, available_depth) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    parent,
                    record.agent.path().last().copied(),
                    owner_job.map(|job| job.get()),
                    *available_depth,
                ],
            )?;
            self.agents.insert(record.agent.path().to_vec(), id);
        }
        Ok(())
    }

    fn subtype(&mut self, db: &Db, entry: Entry, event: &SessionEvent) -> DbResult<()> {
        use SessionEvent as E;
        match event {
            E::SessionStarted { .. }
            | E::TargetsUpserted { .. }
            | E::AgentStarted { .. }
            | E::ModeChanged { .. }
            | E::ModelChanged { .. }
            | E::ModelContext { .. } => self.contract(db, entry, event),
            E::TitleSet { title, source } => db
                .execute(
                    "INSERT INTO title (entry, source, text) VALUES (?1, ?2, ?3)",
                    params![entry.seq, *source, title],
                )
                .map(drop),
            E::Status { message } => db
                .execute(
                    "INSERT INTO entry_text (entry, text) VALUES (?1, ?2)",
                    params![entry.seq, message],
                )
                .map(drop),
            E::TodosReplaced { items } => super::message::todos(db, entry.seq, entry.kind, items),
            E::MessageCommitted { message } => {
                let message = self.message_for(db, entry.agent, message)?;
                db.execute(
                    "INSERT INTO message_commit (entry, message) VALUES (?1, ?2)",
                    params![entry.seq, message],
                )
                .map(drop)
            }
            E::ModelRequested { .. }
            | E::ModelAttemptStarted(_)
            | E::ModelFailed { .. }
            | E::ModelAttemptInterrupted(_)
            | E::ResponseCompleted { .. }
            | E::ModelRecoveryScheduled { .. }
            | E::Usage { .. }
            | E::Compaction { .. }
            | E::CompactionFailed { .. }
            | E::AgentFailed { .. } => self.request(db, entry, event),
            E::JobCreated { .. }
            | E::JobStateChanged { .. }
            | E::JobFinished { .. }
            | E::JobClaimed { .. }
            | E::JobInjected { .. }
            | E::JobMessageDelivered { .. } => self.job(db, entry, event),
            E::ApprovalGranted { grant } => self.grant(db, entry.seq, grant),
            E::ApprovalRevoked { grant } => db
                .execute(
                    "INSERT INTO approval_revocation (entry, grant_entry) VALUES (?1, ?2)",
                    params![entry.seq, *grant],
                )
                .map(drop),
            E::SessionReopened | E::TitleCleared | E::AgentCompleted | E::AgentInterrupted => {
                Ok(())
            }
        }
    }

    fn agent(&mut self, db: &Db, agent: &AgentId) -> DbResult<i64> {
        if let Some(id) = self.agents.get(agent.path()) {
            return Ok(*id);
        }
        let mut id = db
            .query_row(
                "SELECT id FROM agent WHERE parent IS NULL",
                Vec::new(),
                |row| Ok(row.get::<i64>(0)?),
            )?
            .ok_or_else(|| rejected(format!("agent {agent} has not started")))?;
        for segment in agent.path() {
            id = db
                .query_row(
                    "SELECT id FROM agent WHERE parent = ?1 AND child_index = ?2",
                    params![id, *segment],
                    |row| Ok(row.get::<i64>(0)?),
                )?
                .ok_or_else(|| rejected(format!("agent {agent} has not started")))?;
        }
        self.agents.insert(agent.path().to_vec(), id);
        Ok(id)
    }

    pub(super) fn target(&mut self, db: &Db, target: &TargetRef) -> DbResult<i64> {
        if let Some(id) = self.targets.get(target) {
            return Ok(*id);
        }
        db.execute(
            "INSERT INTO target (name) VALUES (?1) ON CONFLICT (name) DO NOTHING",
            params![target],
        )?;
        let id = db
            .query_row(
                "SELECT id FROM target WHERE name = ?1",
                params![target],
                |row| Ok(row.get::<i64>(0)?),
            )?
            .ok_or_else(|| corrupt("target row is missing"))?;
        self.targets.insert(target.clone(), id);
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    // Encode -> decode round trip of every session event kind.
    use serde_json::json;

    use crate::{
        agent::CompactionFault,
        execution::ExecutionLocation,
        identity::JobId,
        job::JobView,
        job::{JobEnd, JobRole, JobState, JobTransition},
        media::{AttachmentRef, ImageFormat, ImageRef, TextRef},
        provider::codec::common::tests::envelope,
        provider::protocol::{
            AssistantItem, Binding, HistoryLifetime, ReplayFormat, ResponseSchema, SystemSegment,
            ToolCall, ToolDefinition, Usage,
        },
        session::{
            AttemptRef, CompactionCheckpoint, CompactionFailure, CompletedOutcome, JobEvent,
            Message, ModelCallOrigin, ModelContext, ModelPurpose, RecordSeq, RuntimeState,
            SessionEvent, StateJob, StateJobKind, TitleSource, Truncation, UserPart,
            db::tests::{Fixture, result, user},
            tests::{child_started, profile},
        },
        target::TargetDefinition,
        target::TargetRef,
        tool::policy::{ApprovalGrant, Capability, ResourceId},
    };

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
        let mut profile = profile();
        profile.profile.reasoning = Some("high".into());
        let agent_context = ModelContext {
            purpose: ModelPurpose::Agent,
            profile: profile.clone(),
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
        // A mode that grants nothing still records the change.
        for (mode, capabilities) in [
            ("look", vec![crate::tool::policy::Capability::Read]),
            ("none", vec![]),
        ] {
            // The first use of a mode pins its definition; a later one names it.
            for first in [true, false] {
                let definition = first.then(|| crate::tool::policy::Mode {
                    capabilities: capabilities.clone(),
                    instructions: (!capabilities.is_empty()).then(|| "Only look.".parse().unwrap()),
                    hint: (!capabilities.is_empty()).then(|| "Read-only.".parse().unwrap()),
                });
                one!(SessionEvent::ModeChanged {
                    mode: crate::session::ModeSelection {
                        name: mode.parse().unwrap(),
                        definition,
                    },
                    capabilities: capabilities.clone(),
                });
            }
        }
        let prompt = one!(SessionEvent::MessageCommitted {
            message: Message::User(vec![
                UserPart::Text {
                    text: "look".into(),
                },
                UserPart::Attachment {
                    attachment: AttachmentRef::Image(png.clone()),
                },
                UserPart::Attachment {
                    attachment: AttachmentRef::Text(TextRef {
                        file: None,
                        blob: notes,
                    }),
                },
                UserPart::ParentInput {
                    text: "parent".into(),
                },
            ]),
        });
        let request = one!(SessionEvent::ModelRequested {
            context,
            checkpoint: None,
            through: Some(prompt.message()),
            tail: vec![user("tail")],
            history_lifetime: HistoryLifetime::Detached,
        });
        let attempt = |attempt| AttemptRef {
            request: request.request(),
            attempt,
        };
        one!(SessionEvent::ModelAttemptStarted(attempt(1)));
        let failed = one!(SessionEvent::ModelFailed {
            attempt: attempt(1),
            failure: crate::agent::Failure::Provider(
                "lost".into(),
                crate::provider::ProviderErrorKind::Transport,
            ),
        });
        // An attempt ends once.
        fixture.reject(
            root.clone(),
            SessionEvent::ModelAttemptInterrupted(attempt(1)),
        );
        one!(SessionEvent::ModelRecoveryScheduled {
            failure: failed,
            delay_millis: 1000,
        });
        one!(SessionEvent::ModelAttemptStarted(attempt(2)));
        let payload = json!({"encrypted": "opaque"});
        let replay = envelope(
            ReplayFormat::Responses,
            "model",
            payload,
            Binding::Conversation,
        );
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
            request: request.request(),
            usage: Usage {
                input_tokens: 10,
                cached_input_tokens: 2,
                cache_write_input_tokens: 1,
                output_tokens: 3,
            },
        });
        one!(SessionEvent::ResponseCompleted {
            attempt: attempt(2),
            message: assistant.message(),
            outcome: CompletedOutcome::Cut(Truncation::MaxTokens),
        });
        let job = JobId::new(1).unwrap();
        one!(SessionEvent::JobCreated {
            job,
            parent: None,
            origin: Some(ModelCallOrigin {
                message: assistant.message(),
                call_id: "b".into(),
            }),
            tool: "read".into(),
            role: JobRole::Tool,
            name: Some("reader".parse().unwrap()),
            arguments: json!({"path": "b"}),
            output_schema: Some(json!({"type": "object"})),
            accepts_input: false,
            background: true,
            location: ExecutionLocation::named("build".parse().unwrap(), "/srv".into()),
        });
        one!(SessionEvent::JobStateChanged {
            job,
            state: JobTransition::Running,
        });
        one!(result("b", "read", vec![png.clone()]));
        one!(result("a", "read", Vec::new()));
        one!(SessionEvent::JobFinished {
            job,
            state: JobEnd::Interrupted,
            diagnostic: None,
            output_diagnostic: None,
            images: vec![png.clone()],
        });
        one!(SessionEvent::JobFinished {
            job,
            state: JobEnd::Cancelled,
            diagnostic: None,
            output_diagnostic: None,
            images: Vec::new(),
        });
        // Every resource kind; a route names journaled target revisions.
        one!(SessionEvent::TargetsUpserted {
            targets: vec![TargetDefinition::test("build", "/srv", None)],
        });
        let path = &crate::tool::policy::PathText::new("/srv/a b/../c").unwrap();
        let build: crate::target::TargetName = "build".parse().unwrap();
        for (capability, resource) in [
            (Capability::Mcp, ResourceId::mcp("server", "tool")),
            (Capability::Read, ResourceId::session("scratch")),
            (
                Capability::Write,
                ResourceId::workspace(&"build".parse().unwrap(), path),
            ),
            (
                Capability::Read,
                ResourceId::path(&"build".parse().unwrap(), path),
            ),
            (
                Capability::Network,
                ResourceId::network(&crate::target::TargetRef::Root, "https://example.test"),
            ),
            (
                Capability::Targets,
                ResourceId::route(build.clone(), vec![(build.clone(), 1)]),
            ),
        ] {
            let grant = one!(SessionEvent::ApprovalGranted {
                grant: ApprovalGrant::descendants(capability, resource),
            });
            one!(SessionEvent::ApprovalRevoked { grant });
        }
        fixture.reject(
            root.clone(),
            SessionEvent::ApprovalGranted {
                grant: ApprovalGrant::exact(
                    Capability::Targets,
                    ResourceId::route(build.clone(), vec![(build.clone(), 2)]),
                ),
            },
        );
        one!(SessionEvent::JobClaimed { job });
        one!(SessionEvent::JobInjected { job });
        one!(SessionEvent::JobMessageDelivered {
            job,
            source: assistant.message(),
            notification: prompt.message(),
        });
        one!(SessionEvent::TodosReplaced {
            items: vec![crate::agent::TodoItem {
                text: "todo".parse().unwrap(),
                status: crate::agent::TodoStatus::InProgress,
            }],
        });
        one!(SessionEvent::TodosReplaced { items: Vec::new() });
        // A compaction summary request and its checkpoint.
        let frontier = RecordSeq::from(fixture.records.len() as u64);
        let summary_context = one!(SessionEvent::ModelContext {
            context: ModelContext {
                purpose: ModelPurpose::Compaction,
                profile,
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
            checkpoint: None,
            through: Some(assistant.message()),
            tail: vec![user("summarize")],
            history_lifetime: HistoryLifetime::Detached,
        });
        let summary_attempt = AttemptRef {
            request: summary.request(),
            attempt: 1,
        };
        one!(SessionEvent::ModelAttemptStarted(summary_attempt));
        let checkpoint = one!(SessionEvent::Compaction {
            checkpoint: CompactionCheckpoint {
                frontier,
                message: Message::User(vec![UserPart::Compaction {
                    text: "summary".into(),
                }]),
                todos: vec![crate::agent::TodoItem {
                    text: "kept".parse().unwrap(),
                    status: crate::agent::TodoStatus::Pending,
                }],
                retained: vec![prompt.message()],
                attempt: summary_attempt,
                before_tokens: 100,
                after_tokens: 10,
            },
        });
        one!(SessionEvent::ModelRequested {
            context,
            checkpoint: Some(checkpoint),
            through: Some(prompt.message()),
            tail: Vec::new(),
            history_lifetime: HistoryLifetime::Extends,
        });
        for (failure, error) in [
            (
                CompactionFailure::BeforeRequest,
                CompactionFault::Checkpoint(crate::session::CheckpointError::StaleTodos),
            ),
            (
                CompactionFailure::Requested(summary.request()),
                CompactionFault::Truncated,
            ),
            (
                CompactionFailure::Attempted(summary_attempt),
                CompactionFault::Continuation("missing field".into()),
            ),
        ] {
            one!(SessionEvent::CompactionFailed { failure, error });
        }
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
            location: ExecutionLocation::root("/workspace".into()),
        });
        // Runtime content names journaled jobs, targets and message sources.
        let progress = crate::job::AgentProgress {
            turns: 2,
            tool_calls: 5,
        };
        let reader = StateJob {
            job,
            kind: StateJobKind::Tool {
                tool: "read".into(),
            },
            name: Some("reader".parse().unwrap()),
            state: JobState::Queued,
            target: None,
            workspace: "/srv".into(),
            age_seconds: 1,
            children: Vec::new(),
        };
        let state = RuntimeState {
            date: "2026-09-23".into(),
            // A sibling follows a job's children.
            jobs: vec![
                StateJob {
                    job: owner,
                    kind: StateJobKind::Agent { progress },
                    name: None,
                    state: JobState::Running,
                    target: Some(TargetRef::Root),
                    workspace: "/workspace".into(),
                    age_seconds: 4,
                    children: vec![reader.clone()],
                },
                reader,
            ],
            todos: vec![crate::agent::TodoItem {
                text: "todo".parse().unwrap(),
                status: crate::agent::TodoStatus::Completed,
            }],
            location: ExecutionLocation::root("/workspace".into()),
        };
        let page = JobView {
            id: Some(job),
            state: Some(JobState::Completed),
            result: Some(json!("line")),
            error: None,
            meta: None,
            presentation: None,
        };
        let events = vec![
            JobEvent::Message(crate::job::AgentMessage {
                id: owner,
                name: Some("worker".parse().unwrap()),
                message: assistant.message(),
                text: "progress".into(),
            }),
            JobEvent::Job(Box::new(JobView {
                id: Some(job),
                state: Some(JobState::Failed),
                result: None,
                error: Some("denied".into()),
                meta: Some(crate::job::views::JobMetadata {
                    parent: Some(owner),
                    tool: Some("read".into()),
                    name: Some("reader".parse().unwrap()),
                    target: Some("build".into()),
                    workspace: Some("/srv".into()),
                    code: Some(crate::tool::DenialCode::PermissionDenied),
                }),
                presentation: Some(crate::job::Presentation {
                    preview: Some(crate::job::output::OutputPreview::Lines(
                        crate::job::output::LinePage {
                            field: Some(crate::job::FieldPointer::result()),
                            lines: crate::job::output::PageLines::Numbered(vec![
                                crate::job::output::NumberedLine {
                                    line: 3,
                                    text: "line".into(),
                                },
                            ]),
                            total_lines: Some(1),
                            next_start: None,
                            next_offset: None,
                        },
                    )),
                    shape: Some(json!({"items": [3, "integer"]})),
                    truncated: Some(vec![crate::job::output::OutputTruncation::Elements {
                        field: "/result/items".parse().unwrap(),
                        shown: 1,
                        total_elements: 3,
                        kept: None,
                    }]),
                    captures: Some(vec![crate::job::output::CaptureDescriptor {
                        field: "/result/stdout".parse().unwrap(),
                        complete: true,
                        output: Some(Box::new(page)),
                    }]),
                    question: None,
                    notice: Some(crate::job::Notice::OutputIncomplete),
                }),
            })),
            JobEvent::Job(Box::new(JobView {
                id: Some(owner),
                state: Some(JobState::Completed),
                result: Some(json!({"answer": 1})),
                error: None,
                meta: None,
                presentation: None,
            })),
        ];
        one!(SessionEvent::MessageCommitted {
            message: Message::User(vec![
                UserPart::State { state },
                UserPart::JobEvents { events },
            ]),
        });
        let location = ExecutionLocation::root("/workspace".into());
        let failed = SessionEvent::AgentFailed {
            failure: crate::agent::Failure::Refused("filtered".into()),
        };
        let started = child_started(Some(owner), location);
        let child_events = [started, failed].map(|event| (child.clone(), event));
        fixture.commit(child_events.into()).unwrap();
        for event in [
            SessionEvent::Status {
                message: "status".into(),
            },
            SessionEvent::TitleSet {
                title: "prompt".into(),
                source: TitleSource::Prompt,
            },
            SessionEvent::TitleSet {
                title: "user".into(),
                source: TitleSource::User,
            },
            SessionEvent::TitleSet {
                title: "generated".into(),
                source: TitleSource::Generated,
            },
            SessionEvent::TitleCleared,
            SessionEvent::SessionReopened,
            SessionEvent::AgentCompleted,
            SessionEvent::AgentInterrupted,
        ] {
            one!(event);
        }
        fixture.assert_round_trip();
    }
}

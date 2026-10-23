//! Decompose session events into normalized rows inside the caller's transaction.
//! Each schema section's module writes the rows of its own events.
use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;

use super::{Db, DbResult, corrupt, params, rejected};
use crate::{
    identity::AgentId,
    session::{EventRecord, SessionEvent},
    target::TargetRef,
};

pub(super) fn json(value: &impl Serialize) -> DbResult<String> {
    serde_json::to_string(value).map_err(|error| corrupt(error.to_string()))
}

/// The entry an event's rows belong to.
#[derive(Clone, Copy)]
pub(super) struct Entry {
    pub seq: u64,
    pub agent: i64,
}

/// Encodes records into one open transaction. Surrogate-key caches are only
/// valid for committed rows; the writer resets them after a rollback.
#[derive(Default)]
pub(in crate::session) struct Encoder {
    agents: HashMap<Vec<u32>, i64>,
    targets: HashMap<TargetRef, i64>,
    /// Each agent's next entry ordinal, seeded from the journal on its first entry.
    ordinals: HashMap<i64, i64>,
}

impl Encoder {
    pub(in crate::session) fn reset(&mut self) {
        self.agents.clear();
        self.targets.clear();
        self.ordinals.clear();
    }

    /// Start the agents `records`, one transaction's, start, ahead of encoding any of
    /// them: every entry, including a session start, references its agent.
    pub(in crate::session) fn start(
        &mut self,
        db: &Db,
        records: &[Arc<EventRecord>],
    ) -> DbResult<()> {
        for record in records {
            if let SessionEvent::SessionStarted { .. } = &record.event {
                db.execute(
                    "INSERT INTO session (singleton, public_id) VALUES (1, ?1)",
                    params![record.agent.session().to_bytes().to_vec()],
                )?;
            }
            self.start_agent(db, record)?;
        }
        Ok(())
    }

    /// Encode `record`, in sequence order after the records ahead of it.
    pub(in crate::session) fn record(&mut self, db: &Db, record: &EventRecord) -> DbResult<()> {
        let kind = record.event.kind();
        let entry = Entry {
            seq: record.sequence.get(),
            agent: self.agent(db, &record.agent)?,
        };
        let ordinal = self.ordinal(db, entry.agent)?;
        db.execute(
            "INSERT INTO entry (seq, public_id, agent, agent_ordinal, created_millis, kind) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                entry.seq,
                record.id.to_bytes().to_vec(),
                entry.agent,
                ordinal,
                record.timestamp_millis,
                kind
            ],
        )
        .and_then(|_| self.subtype(db, entry, &record.event))
        .map_err(|error| match error {
            super::DbError::Sql(error) => {
                rejected(format!("{kind} entry {}: {error}", record.sequence))
            }
            error => error,
        })
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
            E::TodosReplaced { items } => {
                super::message::todos(db, "todo_item", "entry", entry.seq, items)
            }
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
            | E::Usage { .. }
            | E::Compaction { .. }
            | E::CompactionFailed { .. }
            | E::AgentFailed { .. } => self.request(db, entry, event),
            E::JobCreated { .. }
            | E::JobStateChanged { .. }
            | E::JobFinished { .. }
            | E::JobClaimed { .. }
            | E::JobCancelRequested { .. }
            | E::JobReleased { .. }
            | E::JobMessageReleased { .. }
            | E::JobInjected { .. }
            | E::JobMessageDelivered { .. } => self.job(db, entry, event),
            E::TitleCleared | E::AgentCompleted | E::AgentInterrupted => Ok(()),
        }
    }

    /// The ordinal `agent`'s next entry takes.
    fn ordinal(&mut self, db: &Db, agent: i64) -> DbResult<i64> {
        let next = match self.ordinals.entry(agent) {
            std::collections::hash_map::Entry::Occupied(next) => next.into_mut(),
            std::collections::hash_map::Entry::Vacant(slot) => {
                let seeded = super::pages::transcript_length(db, agent)?;
                slot.insert(i64::try_from(seeded).unwrap_or(i64::MAX))
            }
        };
        *next += 1;
        Ok(*next - 1)
    }

    pub(super) fn agent(&mut self, db: &Db, agent: &AgentId) -> DbResult<i64> {
        if let Some(id) = self.agents.get(agent.path()) {
            return Ok(*id);
        }
        let id = agent_row(db, agent)?
            .ok_or_else(|| rejected(format!("agent {agent} has not started")))?;
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

/// `agent`'s row, once it has started.
pub(in crate::session) fn agent_row(db: &Db, agent: &AgentId) -> DbResult<Option<i64>> {
    let mut id = db.query_row(
        "SELECT id FROM agent WHERE parent IS NULL",
        Vec::new(),
        |row| Ok(row.get::<i64>(0)?),
    )?;
    for segment in agent.path() {
        let Some(parent) = id else { break };
        id = db.query_row(
            "SELECT id FROM agent WHERE parent = ?1 AND child_index = ?2",
            params![parent, *segment],
            |row| Ok(row.get::<i64>(0)?),
        )?;
    }
    Ok(id)
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
        let image = fixture.blob(&crate::tests::png_bytes(b"image"));
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
        // A mode that grants nothing still records the change. An entry with a
        // definition records its name's next edition; one without names the latest.
        let edition = |instructions: Option<&str>| crate::tool::policy::Mode {
            capabilities: instructions
                .map(|_| crate::tool::policy::Capability::Read)
                .into_iter()
                .collect(),
            instructions: instructions.map(|text| text.parse().unwrap()),
            hint: instructions.map(|_| "Read-only.".parse().unwrap()),
        };
        let editions = [
            ("look", edition(Some("Only look."))),
            ("none", edition(None)),
            ("look", edition(Some("Look closer."))),
        ];
        for (mode, definition) in editions {
            let capabilities = definition.capabilities.clone();
            for definition in [Some(definition), None] {
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
            from: None,
            tail: vec![user("tail")],
            history_lifetime: HistoryLifetime::Detached,
        });
        let attempt = |attempt| AttemptRef {
            request: request.request(),
            attempt,
        };
        one!(SessionEvent::ModelAttemptStarted(attempt(1)));
        one!(SessionEvent::ModelFailed {
            attempt: attempt(1),
            failure: crate::session::AttemptFailure::Retrying {
                message: "lost".into(),
                kind: crate::provider::ProviderErrorKind::Transport,
                delay: std::time::Duration::from_secs(1).into(),
            },
        });
        // An attempt ends once.
        fixture.reject(
            root.clone(),
            SessionEvent::ModelAttemptInterrupted(attempt(1)),
        );
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
            usage: Usage::prompted(10, 2, 1, 3),
        });
        one!(SessionEvent::ResponseCompleted {
            attempt: attempt(2),
            message: assistant.message(),
            outcome: CompletedOutcome::Cut(Truncation::MaxTokens),
        });
        let job = JobId::new(1).unwrap();
        // A model call's job keeps no copy of its call's arguments, unless they were
        // repaired; a script's call has its own. Equal schemas are stored once.
        let schema = std::sync::Arc::new(json!({"type": "object"}));
        let read = |id, parent, call: Option<&str>, arguments| SessionEvent::JobCreated {
            job: JobId::new(id).unwrap(),
            parent,
            origin: call.map(|call| ModelCallOrigin {
                message: assistant.message(),
                call_id: call.into(),
            }),
            tool: "read".into(),
            role: JobRole::Tool,
            name: Some("reader".parse().unwrap()),
            arguments,
            output_schema: Some(schema.clone()),
            accepts_input: false,
            background: true,
            location: ExecutionLocation::named("build".parse().unwrap(), "/srv".into()),
        };
        one!(read(1, None, Some("b"), json!({"path": "b"})));
        one!(read(3, None, Some("a"), json!({"path": "a/"})));
        one!(read(4, Some(job), None, json!({"path": "b"})));
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
            images: vec![png.clone()],
        });
        one!(SessionEvent::JobFinished {
            job,
            state: JobEnd::Cancelled,
            diagnostic: None,
            images: Vec::new(),
        });
        one!(SessionEvent::TargetsUpserted {
            targets: vec![TargetDefinition::test("build", "/srv", None)],
        });
        one!(SessionEvent::JobClaimed { job });
        one!(SessionEvent::JobCancelRequested { job });
        one!(SessionEvent::JobReleased { job });
        one!(SessionEvent::JobMessageReleased { job });
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
                response_schema: Some(
                    ResponseSchema::new("summary", json!({"type": "object"})).unwrap()
                ),
                ..agent_context
            },
        });
        let summary = one!(SessionEvent::ModelRequested {
            context: summary_context,
            checkpoint: None,
            through: Some(assistant.message()),
            from: None,
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
                trigger: crate::session::CompactionTrigger::Occupancy(100),
            },
        });
        one!(SessionEvent::ModelRequested {
            context,
            checkpoint: Some(checkpoint),
            through: Some(prompt.message()),
            from: None,
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
                meta: None,
                presentation: None,
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
            message: Message::State(state),
        });
        one!(SessionEvent::MessageCommitted {
            message: Message::User(vec![UserPart::JobEvents { events }]),
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
            SessionEvent::AgentCompleted,
            SessionEvent::AgentInterrupted,
        ] {
            one!(event);
        }
        fixture.assert_round_trip();
        let jobs = fixture.db.query(
            "SELECT arguments IS NULL, output_schema IS NOT NULL FROM job ORDER BY id",
            Vec::new(),
            |row| Ok((row.get::<bool>(0)?, row.get::<bool>(1)?)),
        );
        let (from_call, own) = ((true, true), (false, true));
        assert_eq!(jobs.unwrap(), [from_call, (false, false), own, own]);
        let schemas = fixture.db.query(
            "SELECT count(DISTINCT output_schema) FROM job",
            Vec::new(),
            |row| Ok(row.get::<i64>(0)?),
        );
        assert_eq!(schemas.unwrap(), [1]);
        let decoded = fixture.decode().unwrap();
        let schemas: Vec<_> = (decoded.iter())
            .filter_map(|record| match &record.event {
                SessionEvent::JobCreated { output_schema, .. } => output_schema.clone(),
                _ => None,
            })
            .collect();
        assert!(
            schemas
                .windows(2)
                .all(|pair| std::sync::Arc::ptr_eq(&pair[0], &pair[1]))
        );
        // Per-message decode reads what the whole journal's decode does.
        let committed: Vec<_> = (decoded.iter())
            .filter(|record| record.agent == root)
            .filter_map(|record| match &record.event {
                SessionEvent::MessageCommitted { message } => {
                    Some((record.sequence.message(), message.clone()))
                }
                _ => None,
            })
            .collect();
        let commits: Vec<_> = committed.iter().map(|(commit, _)| *commit).collect();
        let by_commit = super::super::message::committed(&fixture.db, &commits).unwrap();
        assert_eq!(by_commit, committed.into_iter().collect());
        let look = fixture.db.query(
            "SELECT mode_version FROM agent_mode WHERE mode_name = 'look' ORDER BY entry",
            Vec::new(),
            |row| Ok(row.get::<i64>(0)?),
        );
        assert_eq!(look.unwrap(), [1, 1, 2, 2]);
    }
}

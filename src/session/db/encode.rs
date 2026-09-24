//! Decompose session events into normalized rows inside the caller's transaction.
use std::collections::HashMap;

use libsql::Value;
use serde::Serialize;

use super::{
    Db, DbResult, JobEventKind, MessageRole, ResponseOutcome, UserPartKind, corrupt,
    diagnostic::Slot, enum_column, params, rejected,
};
use crate::{
    identity::AgentId,
    job::JobEnd,
    media::{AttachmentRef, BlobRef, ImageRef},
    provider::protocol::{AssistantItem, ToolResult},
    session::{
        AttemptRef, CompactionCheckpoint, CompletedOutcome, EntryKind, EventRecord, JobEvent,
        Message, MessageSeq, ModelContext, ProfileSnapshot, RequestSeq, RuntimeState, SessionEvent,
        StateJob, StateJobKind, UserPart,
    },
    target::{TargetDefinition, TargetName},
    tool::policy::{ApprovalGrant, ResourceId},
};

fn json(value: &impl Serialize) -> DbResult<String> {
    serde_json::to_string(value).map_err(|error| corrupt(error.to_string()))
}

fn digest(value: &impl Serialize) -> DbResult<Vec<u8>> {
    use sha2::Digest as _;
    let bytes = serde_json::to_vec(value).map_err(|error| corrupt(error.to_string()))?;
    Ok(sha2::Sha256::digest(bytes).to_vec())
}

#[cfg(unix)]
pub(super) fn path_bytes(path: &std::path::Path) -> Vec<u8> {
    std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()).to_vec()
}

#[cfg(not(unix))]
pub(super) fn path_bytes(path: &std::path::Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

/// Encodes records into one open transaction. Surrogate-key caches are only
/// valid for committed rows; the writer resets them after a rollback.
#[derive(Default)]
pub(in crate::session) struct Encoder {
    agents: HashMap<Vec<u32>, i64>,
}

impl Encoder {
    pub(in crate::session) fn reset(&mut self) {
        self.agents.clear();
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
            let agent = self.agent(db, &record.agent)?;
            db.execute(
                "INSERT INTO entry (seq, public_id, agent, created_millis, kind) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    record.sequence.get(),
                    record.id.to_bytes().to_vec(),
                    agent,
                    record.timestamp_millis,
                    record.event.kind()
                ],
            )
            .and_then(|_| self.subtype(db, record, agent))
            .map_err(|error| match error {
                super::DbError::Sql(error) => rejected(format!(
                    "{} entry {}: {error}",
                    record.event.kind(),
                    record.sequence
                )),
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

    fn subtype(&mut self, db: &Db, record: &EventRecord, agent: i64) -> DbResult<()> {
        let (seq, kind) = (record.sequence.get(), record.event.kind());
        match &record.event {
            SessionEvent::SessionStarted { targets, .. }
            | SessionEvent::TargetsUpserted { targets } => {
                self.targets(db, seq, kind, targets)?;
            }
            SessionEvent::AgentStarted {
                profile,
                mode,
                capabilities,
                location,
                ..
            } => {
                let profile = profile
                    .as_ref()
                    .map(|profile| self.profile(db, profile))
                    .transpose()?;
                let target = self.target(db, location.target.as_str())?;
                db.execute(
                    "INSERT INTO agent_start (entry, profile, location_target, \
                     location_workspace) VALUES (?1, ?2, ?3, ?4)",
                    params![seq, profile, target, path_bytes(&location.workspace)],
                )?;
                if let Some(mode) = mode {
                    self.mode(db, seq, kind, mode)?;
                }
                capabilities_at(db, seq, kind, capabilities)?;
            }
            SessionEvent::ModeChanged { mode, capabilities } => {
                self.mode(db, seq, kind, mode)?;
                capabilities_at(db, seq, kind, capabilities)?;
            }
            SessionEvent::TodosReplaced { items } => todos(db, seq, kind, items)?,
            SessionEvent::ModelChanged { profile } => {
                let profile = self.profile(db, profile)?;
                db.execute(
                    "INSERT INTO model_selection (entry, profile) VALUES (?1, ?2)",
                    params![seq, profile],
                )?;
            }
            SessionEvent::MessageCommitted { message } => {
                let message = self.message_for(db, agent, message)?;
                db.execute(
                    "INSERT INTO message_commit (entry, message) VALUES (?1, ?2)",
                    params![seq, message],
                )?;
            }
            SessionEvent::Status { message: text }
            | SessionEvent::TitleSet { title: text }
            | SessionEvent::AgentFailed { error: text } => {
                db.execute(
                    "INSERT INTO entry_text (entry, kind, text) VALUES (?1, ?2, ?3)",
                    params![seq, kind, text],
                )?;
            }
            SessionEvent::ModelContext { context } => self.context(db, seq, context)?,
            SessionEvent::ModelRequested {
                context,
                checkpoint,
                history: sources,
                tail,
                history_lifetime,
            } => {
                let context_agent = db.query_row(
                    "SELECT e.agent FROM model_context c JOIN entry e ON e.seq = c.entry \
                     WHERE c.entry = ?1",
                    params![*context],
                    |row| Ok(row.get::<i64>(0)?),
                )?;
                if context_agent != Some(agent) {
                    return Err(rejected("request context must belong to the same agent"));
                }
                // History is recorded as its range less the sources it left out.
                let through = sources.last().copied();
                let derived = db.query(
                    "SELECT source FROM (SELECT source, 0 AS part FROM compaction_retained \
                       WHERE compaction = ?1 \
                     UNION ALL SELECT m.entry, 1 FROM message_commit m \
                       JOIN entry e ON e.seq = m.entry WHERE e.agent = ?2 AND m.entry <= ?3 \
                       AND m.entry > coalesce((SELECT frontier FROM compaction WHERE entry = ?1), 0)) \
                     ORDER BY part, source",
                    params![*checkpoint, agent, through],
                    |row| Ok(super::decode::sequence(row.get(0)?).message()),
                )?;
                let sent = |source: &&MessageSeq| sources.binary_search(source).is_ok();
                if !derived.iter().filter(sent).eq(sources) {
                    return Err(rejected(
                        "request history is not from the agent's projected history",
                    ));
                }
                db.execute(
                    "INSERT INTO model_request (entry, context, checkpoint, history_through, \
                     history_lifetime) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![seq, *context, *checkpoint, through, *history_lifetime],
                )?;
                for source in derived.iter().filter(|source| !sent(source)) {
                    db.execute(
                        "INSERT INTO model_request_omitted (request, source) VALUES (?1, ?2)",
                        params![seq, *source],
                    )?;
                }
                for (position, message) in tail.iter().enumerate() {
                    let message = self.message(db, message)?;
                    db.execute(
                        "INSERT INTO model_request_tail (request, position, message) \
                         VALUES (?1, ?2, ?3)",
                        params![seq, position, message],
                    )?;
                }
            }
            SessionEvent::Compaction { checkpoint } => self.compaction(db, seq, checkpoint)?,
            SessionEvent::ModelAttemptStarted(attempt) => {
                db.execute(
                    "INSERT INTO model_attempt (entry, request, attempt) VALUES (?1, ?2, ?3)",
                    params![seq, attempt.request, attempt.attempt],
                )?;
            }
            SessionEvent::ModelFailed {
                attempt,
                error,
                kind: failure,
            } => {
                outcome(db, seq, kind, *attempt)?;
                db.execute(
                    "INSERT INTO model_failure (entry, failure, error) VALUES (?1, ?2, ?3)",
                    params![seq, *failure, error],
                )?;
            }
            SessionEvent::ModelAttemptInterrupted(attempt) => outcome(db, seq, kind, *attempt)?,
            SessionEvent::ResponseCompleted {
                attempt,
                message,
                outcome: ended,
            } => {
                outcome(db, seq, kind, *attempt)?;
                let message = db
                    .query_row(
                        "SELECT c.message FROM message_commit c \
                         JOIN message m ON m.id = c.message \
                         WHERE c.entry = ?1 AND m.role = ?2",
                        params![*message, MessageRole::Assistant],
                        |row| Ok(row.get::<i64>(0)?),
                    )?
                    .ok_or_else(|| rejected("response message is not a committed message"))?;
                let (outcome, cut) = match ended {
                    CompletedOutcome::Answer => (ResponseOutcome::Answer, None),
                    CompletedOutcome::ToolUse => (ResponseOutcome::ToolUse, None),
                    CompletedOutcome::Cut(truncation) => (ResponseOutcome::Cut, Some(*truncation)),
                };
                db.execute(
                    "INSERT INTO model_response (entry, message, outcome, cut_reason) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![seq, message, outcome, cut],
                )?;
            }
            SessionEvent::ModelRecoveryScheduled {
                failure,
                delay_millis,
            } => {
                db.execute(
                    "INSERT INTO model_recovery (entry, failure, delay_millis) VALUES (?1, ?2, ?3)",
                    params![seq, *failure, *delay_millis],
                )?;
            }
            SessionEvent::CompactionSkipped { attempt, reason } => {
                compaction_outcome(db, seq, kind, Some(attempt.request), Some(*attempt), reason)?;
            }
            SessionEvent::CompactionFailed { failure, error } => {
                compaction_outcome(db, seq, kind, failure.request(), failure.attempt(), error)?;
            }
            SessionEvent::Usage { request, usage } => {
                db.execute(
                    "INSERT INTO usage (entry, request, input_tokens, cached_input_tokens, \
                     output_tokens) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        seq,
                        *request,
                        usage.input_tokens,
                        usage.cached_input_tokens,
                        usage.output_tokens
                    ],
                )?;
            }
            SessionEvent::JobCreated {
                job,
                parent,
                origin,
                tool,
                role,
                name,
                arguments,
                output_schema,
                accepts_input,
                background,
                location,
            } => {
                let origin = origin
                    .as_ref()
                    .map(|origin| {
                        db.query_row(
                            "SELECT c.item FROM tool_call c \
                             JOIN assistant_item i ON i.id = c.item \
                             JOIN message_commit m ON m.message = i.message \
                             WHERE m.entry = ?1 AND c.call_id = ?2",
                            params![origin.message, &origin.call_id],
                            |row| Ok(row.get::<i64>(0)?),
                        )?
                        .ok_or_else(|| rejected("job origin names no committed tool call"))
                    })
                    .transpose()?;
                let target = self.target(db, location.target.as_str())?;
                db.execute(
                    "INSERT INTO job (id, created, parent, origin_call, tool, name, role, \
                     arguments, output_schema, accepts_input, background, location_target, \
                     location_workspace) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    params![
                        job.get(),
                        seq,
                        parent.map(|parent| parent.get()),
                        origin,
                        tool,
                        name.clone().map(String::from),
                        *role,
                        json(arguments)?,
                        output_schema.as_ref().map(json).transpose()?,
                        *accepts_input,
                        *background,
                        target,
                        path_bytes(&location.workspace),
                    ],
                )?;
                db.execute(
                    "INSERT INTO job_run (job, generation, started) VALUES (?1, 0, ?2)",
                    params![job.get(), seq],
                )?;
            }
            SessionEvent::ApprovalGranted { grant } => self.grant(db, seq, grant)?,
            SessionEvent::ApprovalRevoked { grant } => {
                db.execute(
                    "INSERT INTO approval_revocation (entry, grant_entry) VALUES (?1, ?2)",
                    params![seq, *grant],
                )?;
            }
            SessionEvent::JobStateChanged { job, state } => {
                db.execute(
                    "INSERT INTO job_transition (entry, job, state) VALUES (?1, ?2, ?3)",
                    params![seq, job.get(), *state],
                )?;
                // A finished job that runs again starts its next generation.
                db.execute(
                    "INSERT INTO job_run (job, generation, started) \
                     SELECT ?1, (SELECT max(generation) + 1 FROM job_run WHERE job = ?1), ?2 \
                     WHERE ?3 = 'running' \
                       AND (SELECT max(entry) FROM job_finish WHERE job = ?1) > coalesce( \
                         (SELECT max(entry) FROM job_transition WHERE job = ?1 AND entry < ?2), 0)",
                    params![job.get(), seq, *state],
                )?;
            }
            SessionEvent::JobFinished {
                job,
                state,
                diagnostic,
                output_diagnostic,
                images,
            } => {
                // Retained jobs reopen after a running reset; only an interrupted
                // outcome may be followed directly by cancellation.
                let previous = db.query_row(
                    "SELECT f.state, f.entry > coalesce((SELECT max(t.entry) FROM job_transition t \
                     WHERE t.job = f.job AND t.state = 'running'), 0) \
                     FROM job_finish f WHERE f.job = ?1 ORDER BY f.entry DESC LIMIT 1",
                    params![job.get()],
                    |row| Ok((enum_column::<JobEnd>(row, 0)?, row.get::<bool>(1)?)),
                )?;
                if let Some((previous, current)) = previous
                    && (current || previous == JobEnd::Cancelled)
                {
                    let reopened =
                        current && previous == JobEnd::Interrupted && *state == JobEnd::Cancelled;
                    if !reopened {
                        return Err(rejected(format!(
                            "duplicate or invalid terminal event for job {job}"
                        )));
                    }
                }
                db.execute(
                    "INSERT INTO job_finish (entry, job, state) VALUES (?1, ?2, ?3)",
                    params![seq, job.get(), *state],
                )?;
                for (slot, diagnostic) in [
                    (Slot::Diagnostic, diagnostic),
                    (Slot::OutputDiagnostic, output_diagnostic),
                ] {
                    if let Some(diagnostic) = diagnostic {
                        self.diagnostic(db, seq, slot, diagnostic)?;
                    }
                }
                for (position, image) in images.iter().enumerate() {
                    image_row(db, "job_finish_image", "finish", seq, position, image)?;
                }
            }
            SessionEvent::JobClaimed { job } => delivery(db, seq, kind, *job, None, None)?,
            SessionEvent::JobInjected { job } => delivery(db, seq, kind, *job, None, None)?,
            SessionEvent::JobMessageDelivered {
                job,
                source,
                notification,
            } => delivery(db, seq, kind, *job, Some(*notification), Some(*source))?,
            SessionEvent::AgentCompleted | SessionEvent::AgentInterrupted => {}
        }
        Ok(())
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

    fn mode(
        &self,
        db: &Db,
        seq: u64,
        kind: EntryKind,
        mode: &crate::session::ModeSelection,
    ) -> DbResult<()> {
        let name = mode.name.as_str();
        if let Some(definition) = &mode.definition {
            pin_mode(db, seq, kind, name, definition)?;
        }
        let inserted = db.execute(
            "INSERT INTO agent_mode (entry, kind, mode) \
             SELECT ?1, ?2, id FROM mode WHERE name = ?3",
            params![seq, kind, name],
        )?;
        if inserted == 0 {
            return Err(rejected(format!(
                "mode {name:?} is not pinned by the session"
            )));
        }
        Ok(())
    }

    fn grant(&self, db: &Db, seq: u64, grant: &ApprovalGrant) -> DbResult<()> {
        let target = match &grant.resource {
            ResourceId::Workspace { target, .. }
            | ResourceId::Path { target, .. }
            | ResourceId::Network { target, .. }
            | ResourceId::Route {
                destination: target,
                ..
            } => Some(self.target(db, target)?),
            ResourceId::Session { .. } | ResourceId::Mcp { .. } => None,
        };
        let (path, origin, session, server, tool) = match &grant.resource {
            ResourceId::Workspace { path, .. } => (Some(path), None, None, None, None),
            ResourceId::Network { origin, .. } => (None, Some(origin), None, None, None),
            ResourceId::Session { name } => (None, None, Some(name), None, None),
            ResourceId::Mcp { server, tool } => (None, None, None, Some(server), Some(tool)),
            ResourceId::Path { .. } | ResourceId::Route { .. } => (None, None, None, None, None),
        };
        db.execute(
            "INSERT INTO approval_grant (entry, capability, resource_kind, target, path, \
             origin, session_name, mcp_server, mcp_tool, coverage) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                seq,
                grant.capability,
                grant.resource.kind(),
                target,
                path,
                origin,
                session,
                server,
                tool,
                grant.coverage
            ],
        )?;
        match &grant.resource {
            ResourceId::Path { components, .. } => {
                for (position, component) in components.iter().enumerate() {
                    db.execute(
                        "INSERT INTO approval_grant_path_component (grant_entry, position, \
                         component) VALUES (?1, ?2, ?3)",
                        params![seq, position, component],
                    )?;
                }
            }
            ResourceId::Route { hops, .. } => {
                for (position, (name, revision)) in hops.iter().enumerate() {
                    let target = self.target(db, name)?;
                    db.execute(
                        "INSERT INTO approval_grant_route_hop (grant_entry, position, target, \
                         revision) VALUES (?1, ?2, ?3, ?4)",
                        params![seq, position, target, *revision],
                    )?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn state(&self, db: &Db, part: i64, state: &RuntimeState) -> DbResult<()> {
        let target = self.target(db, state.location.target.as_str())?;
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
        &self,
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
                .map(|target| self.target(db, target.as_str()))
                .transpose()?;
            let progress = match &job.kind {
                StateJobKind::Agent { progress } => Some(*progress),
                StateJobKind::Tool { .. } => None,
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
                    job.kind.tool(),
                    job.name.clone(),
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

    fn job_events(&self, db: &Db, part: i64, events: &[JobEvent]) -> DbResult<()> {
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
                            message.name.clone(),
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

    pub(super) fn target(&self, db: &Db, name: &str) -> DbResult<i64> {
        db.execute(
            "INSERT INTO target (name) VALUES (?1) ON CONFLICT (name) DO NOTHING",
            params![name],
        )?;
        db.query_row(
            "SELECT id FROM target WHERE name = ?1",
            params![name],
            |row| Ok(row.get::<i64>(0)?),
        )?
        .ok_or_else(|| corrupt("target row is missing"))
    }

    fn targets(
        &self,
        db: &Db,
        seq: u64,
        kind: EntryKind,
        targets: &[TargetDefinition],
    ) -> DbResult<()> {
        for definition in targets {
            let target = self.target(db, definition.name.as_str())?;
            let named = |name: &Option<TargetName>| {
                (name.as_ref())
                    .map(|name| self.target(db, name.as_str()))
                    .transpose()
            };
            let (via, origin) = (named(&definition.via)?, named(&definition.origin)?);
            let ssh = &definition.ssh;
            let key = match &ssh.auth {
                crate::target::TargetAuth::Key { path } => Some(path_bytes(path)),
                _ => None,
            };
            let revision = db.insert(
                "INSERT INTO target_revision (target, entry, kind, revision, source, host, \
                 workspace, ssh_user, ssh_port, ssh_auth, ssh_key_path, ssh_external_agent, \
                 via, origin) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    target,
                    seq,
                    kind,
                    definition.revision,
                    definition.source,
                    &definition.host,
                    path_bytes(&definition.workspace),
                    ssh.user.clone(),
                    ssh.port.map(std::num::NonZeroU16::get),
                    ssh.auth.kind(),
                    key,
                    ssh.external_agent,
                    via,
                    origin,
                ],
            )?;
            for (key, value) in &ssh.options {
                db.execute(
                    "INSERT INTO target_ssh_option (revision, key, value) VALUES (?1, ?2, ?3)",
                    params![revision, key, value],
                )?;
            }
        }
        Ok(())
    }

    fn profile(&self, db: &Db, snapshot: &ProfileSnapshot) -> DbResult<i64> {
        let digest = digest(snapshot)?;
        let profile = &snapshot.profile;
        db.execute(
            "INSERT INTO model_profile (name, provider, model, reasoning, max_context, \
             max_output, supports_images, state_mode, hint, digest) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) ON CONFLICT (digest) DO NOTHING",
            params![
                &snapshot.name,
                &profile.provider,
                &profile.model,
                profile.reasoning.clone(),
                profile.max_context,
                profile.max_output,
                profile.supports_images,
                profile.state_mode,
                profile.hint.clone(),
                digest.clone(),
            ],
        )?;
        by_digest(db, "model_profile", digest)
    }

    fn context(&self, db: &Db, seq: u64, context: &ModelContext) -> DbResult<()> {
        let profile = self.profile(db, &context.profile)?;
        let prompt_digest = digest(&context.system)?;
        let inserted = db.execute(
            "INSERT INTO system_prompt (digest) VALUES (?1) ON CONFLICT (digest) DO NOTHING",
            params![prompt_digest.clone()],
        )? == 1;
        let prompt = by_digest(db, "system_prompt", prompt_digest)?;
        if inserted {
            for (position, segment) in context.system.iter().enumerate() {
                db.execute(
                    "INSERT INTO system_segment (prompt, position, text, cache) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![prompt, position, &segment.text, segment.cache],
                )?;
            }
        }
        let (schema_name, schema) = match &context.response_schema {
            Some(schema) => (Some(schema.name.clone()), Some(json(&schema.schema)?)),
            None => (None, None),
        };
        db.execute(
            "INSERT INTO model_context (entry, purpose, profile, system_prompt, \
             response_schema_name, response_schema) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![seq, context.purpose, profile, prompt, schema_name, schema],
        )?;
        for (position, tool) in context.tools.iter().enumerate() {
            let digest = digest(tool)?;
            db.execute(
                "INSERT INTO tool_definition (name, description, input_schema, digest) \
                 VALUES (?1, ?2, ?3, ?4) ON CONFLICT (digest) DO NOTHING",
                params![
                    &tool.name,
                    &tool.description,
                    json(&tool.input_schema)?,
                    digest.clone()
                ],
            )?;
            let tool = by_digest(db, "tool_definition", digest)?;
            db.execute(
                "INSERT INTO model_context_tool (context, position, tool) VALUES (?1, ?2, ?3)",
                params![seq, position, tool],
            )?;
        }
        Ok(())
    }

    fn compaction(&self, db: &Db, seq: u64, checkpoint: &CompactionCheckpoint) -> DbResult<()> {
        let message = self.message(db, &checkpoint.message)?;
        outcome(db, seq, EntryKind::Compaction, checkpoint.attempt)?;
        db.execute(
            "INSERT INTO compaction (entry, frontier, message, before_tokens, after_tokens) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                seq,
                checkpoint.frontier,
                message,
                checkpoint.before_tokens,
                checkpoint.after_tokens
            ],
        )?;
        for source in &checkpoint.retained {
            db.execute(
                "INSERT INTO compaction_retained (compaction, source) VALUES (?1, ?2)",
                params![seq, *source],
            )?;
        }
        todos(db, seq, EntryKind::Compaction, &checkpoint.todos)
    }

    /// Insert a message committed by `agent`; a tool result binds to that agent's
    /// latest committed call of the same id that has no result yet.
    fn message_for(&self, db: &Db, agent: i64, message: &Message) -> DbResult<i64> {
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

    fn message(&self, db: &Db, message: &Message) -> DbResult<i64> {
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
                        UserPart::JobEvents { events } => self.job_events(db, row, events)?,
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
}

fn assistant_item(db: &Db, message: i64, item: &AssistantItem) -> DbResult<()> {
    let kind = item.kind();
    let id = db.insert(
        "INSERT INTO assistant_item (message, position, provider_id, kind) \
         VALUES (?1, ?2, ?3, ?4)",
        params![message, item.position().get(), item.id().as_str(), kind],
    )?;
    if let Some(replay) = item.replay() {
        db.execute(
            "INSERT INTO reasoning_replay (item, protocol, model, scope, payload, binding) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                &replay.provenance.protocol,
                &replay.provenance.model,
                replay.provenance.scope.as_str(),
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
                        block.id.as_str(),
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

fn image_row(
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

fn by_digest(db: &Db, table: &str, digest: Vec<u8>) -> DbResult<i64> {
    db.query_row(
        &format!("SELECT id FROM {table} WHERE digest = ?1"),
        params![digest],
        |row| Ok(row.get::<i64>(0)?),
    )?
    .ok_or_else(|| corrupt(format!("{table} row is missing")))
}

fn model_attempt(db: &Db, attempt: AttemptRef) -> DbResult<i64> {
    db.query_row(
        "SELECT entry FROM model_attempt WHERE request = ?1 AND attempt = ?2",
        params![attempt.request, attempt.attempt],
        |row| Ok(row.get::<i64>(0)?),
    )?
    .ok_or_else(|| {
        rejected(format!(
            "request {} has no attempt {}",
            attempt.request, attempt.attempt
        ))
    })
}

/// Pin a mode's definition to the entry that first uses it.
fn pin_mode(
    db: &Db,
    seq: u64,
    kind: EntryKind,
    name: &str,
    mode: &crate::tool::policy::Mode,
) -> DbResult<()> {
    db.execute(
        "INSERT INTO mode (entry, kind, name, instructions, hint) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            seq,
            kind,
            name,
            mode.instructions.as_deref(),
            mode.hint.as_deref()
        ],
    )?;
    for capability in &mode.capabilities {
        db.execute(
            "INSERT INTO mode_capability (mode, capability) SELECT id, ?2 FROM mode WHERE name = ?1",
            params![name, *capability],
        )?;
    }
    Ok(())
}

fn capabilities_at(
    db: &Db,
    seq: u64,
    kind: EntryKind,
    capabilities: &[crate::tool::policy::Capability],
) -> DbResult<()> {
    for capability in capabilities {
        db.execute(
            "INSERT INTO agent_capability (entry, kind, capability) VALUES (?1, ?2, ?3)",
            params![seq, kind, *capability],
        )?;
    }
    Ok(())
}

fn todos(db: &Db, seq: u64, kind: EntryKind, items: &[crate::agent::TodoItem]) -> DbResult<()> {
    for (position, item) in items.iter().enumerate() {
        db.execute(
            "INSERT INTO todo_item (entry, kind, position, text, status) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![seq, kind, position, &item.text, item.status],
        )?;
    }
    Ok(())
}

fn delivery(
    db: &Db,
    seq: u64,
    kind: EntryKind,
    job: crate::identity::JobId,
    notification: Option<MessageSeq>,
    source: Option<MessageSeq>,
) -> DbResult<()> {
    db.execute(
        "INSERT INTO job_delivery (entry, kind, job, notification, source) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![seq, kind, job.get(), notification, source],
    )
    .map(drop)
}

/// Record that `attempt` ended with entry `seq`.
fn outcome(db: &Db, seq: u64, kind: EntryKind, attempt: AttemptRef) -> DbResult<()> {
    let attempt = model_attempt(db, attempt)?;
    db.execute(
        "INSERT INTO attempt_outcome (entry, kind, attempt) VALUES (?1, ?2, ?3)",
        params![seq, kind, attempt],
    )
    .map(drop)
}

/// A present attempt must exist on `request`.
fn compaction_outcome(
    db: &Db,
    seq: u64,
    kind: EntryKind,
    request: Option<RequestSeq>,
    attempt: Option<AttemptRef>,
    reason: &str,
) -> DbResult<()> {
    let attempt = attempt
        .map(|attempt| model_attempt(db, attempt))
        .transpose()?;
    db.execute(
        "INSERT INTO compaction_outcome (entry, kind, request, attempt, reason) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![seq, kind, request, attempt, reason],
    )
    .map(drop)
}

#[cfg(test)]
mod tests {
    // Encode -> decode round trip of every session event kind.
    use serde_json::json;

    use crate::{
        execution::ExecutionLocation,
        identity::JobId,
        job::JobView,
        job::{JobEnd, JobRole, JobState, JobTransition},
        media::{AttachmentRef, ImageFormat, ImageRef, TextRef},
        provider::protocol::{
            AssistantItem, Binding, HistoryLifetime, Provenance, Replay, ResponseSchema, Scope,
            SystemSegment, ToolCall, ToolDefinition, Usage,
        },
        session::{
            AttemptRef, CompactionCheckpoint, CompactionFailure, CompletedOutcome, JobEvent,
            Message, ModelCallOrigin, ModelContext, ModelFailureKind, ModelPurpose, RecordSeq,
            RuntimeState, SessionEvent, StateJob, StateJobKind, Truncation, UserPart,
            db::tests::{Fixture, result, user},
            fixture::{child_started, profile},
        },
        target::TargetDefinition,
        target::TargetRef,
        tool::policy::{ApprovalGrant, Capability, ResourceId},
    };

    #[test]
    fn request_history_may_leave_out_projected_sources() {
        let mut fixture = Fixture::new();
        let root = fixture.start("/workspace");
        let context = fixture.one(
            root.clone(),
            SessionEvent::ModelContext {
                context: ModelContext::test(ModelPurpose::Agent, profile()),
            },
        );
        let mut commit = |text| {
            let message = user(text);
            fixture.one(root.clone(), SessionEvent::MessageCommitted { message })
        };
        let (first, _skipped, last) = (commit("first"), commit("skipped"), commit("last"));
        for history in [
            vec![first.message(), last.message()],
            vec![last.message()],
            Vec::new(),
        ] {
            fixture.one(
                root.clone(),
                SessionEvent::ModelRequested {
                    context,
                    checkpoint: None,
                    history,
                    tail: Vec::new(),
                    history_lifetime: HistoryLifetime::Detached,
                },
            );
        }
        fixture.assert_round_trip();
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
        // A mode that grants nothing still records the change.
        for (mode, capabilities) in [
            ("look", vec![crate::tool::policy::Capability::Read]),
            ("none", vec![]),
        ] {
            // The first use of a mode pins its definition; a later one names it.
            for first in [true, false] {
                let definition = first.then(|| crate::tool::policy::Mode {
                    capabilities: capabilities.clone(),
                    instructions: (!capabilities.is_empty()).then(|| "Only look.".into()),
                    hint: (!capabilities.is_empty()).then(|| "Read-only.".into()),
                });
                one!(SessionEvent::ModeChanged {
                    mode: crate::session::ModeSelection {
                        name: mode.into(),
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
        // History is the agent's projected history; nothing else can be named.
        fixture.reject(
            root.clone(),
            SessionEvent::ModelRequested {
                context,
                checkpoint: None,
                history: vec![context.message()],
                tail: Vec::new(),
                history_lifetime: HistoryLifetime::Detached,
            },
        );
        let request = one!(SessionEvent::ModelRequested {
            context,
            checkpoint: None,
            history: vec![prompt.message()],
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
            error: "lost".into(),
            kind: ModelFailureKind::Error,
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
        let replay = Replay {
            provenance: Provenance {
                protocol: "responses".into(),
                model: "model".into(),
                scope: Scope::try_from("reasoning".to_owned()).unwrap(),
            },
            payload: json!({"encrypted": "opaque"}),
            binding: Binding::Conversation,
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
            request: request.request(),
            usage: Usage {
                input_tokens: 10,
                cached_input_tokens: 2,
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
        let path = std::path::Path::new("/srv/a b/../c");
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
                ResourceId::route("build", vec![("build".into(), 1)]),
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
                    ResourceId::route("build", vec![("build".into(), 2)]),
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
                text: "todo".into(),
                status: crate::agent::TodoStatus::InProgress,
            }],
        });
        one!(SessionEvent::TodosReplaced { items: Vec::new() });
        // A compaction summary request and its checkpoint.
        let frontier = RecordSeq::from(fixture.records.len() as u64);
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
            checkpoint: None,
            history: vec![prompt.message(), assistant.message()],
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
                    text: "kept".into(),
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
            history: vec![prompt.message()],
            tail: Vec::new(),
            history_lifetime: HistoryLifetime::Extends,
        });
        for failure in [
            CompactionFailure::BeforeRequest,
            CompactionFailure::Requested(summary.request()),
            CompactionFailure::Attempted(summary_attempt),
        ] {
            one!(SessionEvent::CompactionFailed {
                failure,
                error: "failed".into(),
            });
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
        let state = RuntimeState {
            date: "2026-09-23".into(),
            jobs: vec![StateJob {
                job: owner,
                kind: StateJobKind::Agent { progress },
                name: None,
                state: JobState::Running,
                target: Some(TargetRef::Root),
                workspace: "/workspace".into(),
                age_seconds: 4,
                children: vec![StateJob {
                    job,
                    kind: StateJobKind::Tool {
                        tool: "read".into(),
                    },
                    name: Some("reader".into()),
                    state: JobState::Queued,
                    target: None,
                    workspace: "/srv".into(),
                    age_seconds: 1,
                    children: Vec::new(),
                }],
            }],
            todos: vec![crate::agent::TodoItem {
                text: "todo".into(),
                status: crate::agent::TodoStatus::Completed,
            }],
            location: ExecutionLocation::root("/workspace".into()),
        };
        let page = JobView {
            id: Some(job),
            state: JobState::Completed,
            has_result: true,
            result: json!("line"),
            error: None,
            meta: None,
            presentation: None,
        };
        let events = vec![
            JobEvent::Message(crate::job::AgentMessage {
                id: owner,
                name: Some("worker".into()),
                message: assistant.message(),
                text: "progress".into(),
            }),
            JobEvent::Job(Box::new(JobView {
                id: Some(job),
                state: JobState::Failed,
                has_result: false,
                result: json!(null),
                error: Some("denied".into()),
                meta: Some(crate::job::JobMetadata {
                    parent: Some(owner),
                    tool: Some("read".into()),
                    name: Some("reader".into()),
                    target: Some("build".into()),
                    workspace: Some("/srv".into()),
                    last_message: Some(assistant.message()),
                    code: Some(crate::tool::DenialCode::PermissionDenied),
                    executed: Some(false),
                }),
                presentation: Some(crate::job::Presentation {
                    preview: Some(crate::job::output::OutputPreview {
                        field: "/result".into(),
                        lines: vec!["line".into()],
                        total_lines: Some(1),
                        next_start: None,
                        next_offset: 0,
                    }),
                    truncated: Vec::new(),
                    captures: vec![crate::job::output::CaptureDescriptor {
                        field: "/result/stdout".parse().unwrap(),
                        kind: crate::job::CaptureKind::Text,
                        complete: true,
                        output: Some(Box::new(page)),
                    }],
                    question: None,
                    notice: Some("Output incomplete.".into()),
                }),
            })),
            JobEvent::Job(Box::new(JobView {
                id: Some(owner),
                state: JobState::Completed,
                has_result: true,
                result: json!({"answer": 1}),
                error: None,
                meta: Some(crate::job::JobMetadata::default()),
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
            error: "failed".into(),
        };
        let started = child_started(Some(owner), location);
        let child_events = [started, failed].map(|event| (child.clone(), event));
        fixture.commit(child_events.into()).unwrap();
        for event in [
            SessionEvent::Status {
                message: "status".into(),
            },
            SessionEvent::TitleSet {
                title: "title".into(),
            },
            SessionEvent::AgentCompleted,
            SessionEvent::AgentInterrupted,
        ] {
            one!(event);
        }
        fixture.assert_round_trip();
    }
}

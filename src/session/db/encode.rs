//! Decompose session events into normalized rows inside the caller's transaction.

use std::collections::HashMap;

use libsql::Value;
use serde::Serialize;

use super::{Db, DbResult, corrupt, params, rejected};
use crate::{
    identity::AgentId,
    media::{AttachmentRef, BlobRef, ImageRef},
    provider::protocol::{
        AssistantItem, BlockContent, Message, StopReason, ToolResult, UserContent,
    },
    session::{CompactionCheckpoint, EventRecord, ModelContext, ProfileSnapshot, SessionEvent},
    target::TargetDefinition,
};

/// The serde spelling of a unit enum variant, which the schema's CHECK constraints use.
fn variant(value: &impl Serialize) -> DbResult<String> {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(text)) => Ok(text),
        _ => Err(corrupt("enum value has no plain text spelling")),
    }
}

fn json(value: &impl Serialize) -> DbResult<String> {
    serde_json::to_string(value).map_err(|error| corrupt(error.to_string()))
}

fn digest(value: &impl Serialize) -> DbResult<Vec<u8>> {
    use sha2::Digest as _;
    let bytes = serde_json::to_vec(value).map_err(|error| corrupt(error.to_string()))?;
    Ok(sha2::Sha256::digest(bytes).to_vec())
}

#[cfg(unix)]
fn path_bytes(path: &std::path::Path) -> Vec<u8> {
    std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()).to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &std::path::Path) -> Vec<u8> {
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
                        params![capability.as_str()],
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
                    record.sequence,
                    record.id.to_bytes().to_vec(),
                    agent,
                    record.timestamp_millis,
                    kind(&record.event)
                ],
            )
            .and_then(|_| self.subtype(db, record, agent))
            .map_err(|error| match error {
                super::DbError::Sql(error) => rejected(format!(
                    "{} entry {}: {error}",
                    kind(&record.event),
                    record.sequence
                )),
                error => error,
            })?;
        }
        Ok(())
    }

    fn start_agent(&mut self, db: &Db, record: &EventRecord) -> DbResult<()> {
        if let SessionEvent::AgentStarted {
            parent,
            owner_job,
            available_depth,
            ..
        } = &record.event
        {
            if record.agent.parent() != *parent {
                return Err(rejected("agent start parent differs from its identity"));
            }
            let parent = parent
                .as_ref()
                .map(|parent| self.agent(db, parent))
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
        let (seq, kind) = (record.sequence, kind(&record.event));
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
                let target = self.target(db, &location.target)?;
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
                history,
                tail,
                history_lifetime,
                purpose,
            } => {
                let first_is_checkpoint = match history.first() {
                    Some(first) => db
                        .query_row(
                            "SELECT 1 FROM compaction WHERE entry = ?1",
                            params![*first],
                            |_| Ok(()),
                        )?
                        .is_some(),
                    None => false,
                };
                let (checkpoint, sources) = if first_is_checkpoint {
                    (Some(history[0]), &history[1..])
                } else {
                    (None, &history[..])
                };
                // History is recorded as its range less the sources it left out.
                let through = sources.last().copied();
                let derived = db.query(
                    "SELECT source FROM (SELECT source, 0 AS part FROM compaction_retained \
                       WHERE compaction = ?1 \
                     UNION ALL SELECT m.entry, 1 FROM message_commit m \
                       JOIN entry e ON e.seq = m.entry WHERE e.agent = ?2 AND m.entry <= ?3 \
                       AND m.entry > coalesce((SELECT frontier FROM compaction WHERE entry = ?1), 0)) \
                     ORDER BY part, source",
                    params![checkpoint, agent, through],
                    |row| Ok(row.get::<u64>(0)?),
                )?;
                let sent = |source: &&u64| sources.binary_search(source).is_ok();
                if !derived.iter().filter(sent).eq(sources) {
                    return Err(rejected(
                        "request history is not from the agent's projected history",
                    ));
                }
                db.execute(
                    "INSERT INTO model_request (entry, context, purpose, checkpoint, \
                     history_through, history_lifetime) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        seq,
                        *context,
                        variant(purpose)?,
                        checkpoint,
                        through,
                        variant(history_lifetime)?
                    ],
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
            SessionEvent::ModelAttemptStarted { request, attempt } => {
                db.execute(
                    "INSERT INTO model_attempt (entry, request, attempt) VALUES (?1, ?2, ?3)",
                    params![seq, *request, *attempt],
                )?;
            }
            SessionEvent::ModelFailed {
                request,
                attempt,
                error,
                kind,
            } => {
                outcome(db, seq, "model_failed", *request, *attempt)?;
                db.execute(
                    "INSERT INTO model_failure (entry, failure, error) VALUES (?1, ?2, ?3)",
                    params![seq, variant(kind)?, error],
                )?;
            }
            SessionEvent::ModelAttemptInterrupted { request, attempt } => {
                outcome(db, seq, kind, *request, *attempt)?;
            }
            SessionEvent::ResponseCompleted {
                request,
                attempt,
                message,
                stop_reason,
            } => {
                outcome(db, seq, kind, *request, *attempt)?;
                let message = message
                    .map(|entry| {
                        db.query_row(
                            "SELECT c.message FROM message_commit c \
                             JOIN message m ON m.id = c.message \
                             WHERE c.entry = ?1 AND m.role = 'assistant'",
                            params![entry],
                            |row| Ok(row.get::<i64>(0)?),
                        )?
                        .ok_or_else(|| rejected("response message is not a committed message"))
                    })
                    .transpose()?;
                let (reason, other) = match stop_reason {
                    StopReason::Other(other) => ("other".to_owned(), Some(other.clone())),
                    reason => (variant(reason)?, None),
                };
                db.execute(
                    "INSERT INTO model_response (entry, message, stop_reason, stop_other) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![seq, message, reason, other],
                )?;
            }
            SessionEvent::ModelRecoveryScheduled {
                request,
                attempt,
                delay_millis,
                error,
            } => {
                let failed = attempt
                    .checked_sub(1)
                    .ok_or_else(|| rejected("recovery must follow a failed attempt"))?;
                let failed = model_attempt(db, *request, failed)?;
                let (failure, failure_error) = db
                    .query_row(
                        "SELECT f.entry, f.error FROM model_failure f \
                         JOIN attempt_outcome o ON o.entry = f.entry WHERE o.attempt = ?1",
                        params![failed],
                        |row| Ok((row.get::<i64>(0)?, row.get::<String>(1)?)),
                    )?
                    .ok_or_else(|| rejected("recovery must follow a failed attempt"))?;
                if &failure_error != error {
                    return Err(rejected("recovery error differs from its failure"));
                }
                db.execute(
                    "INSERT INTO model_recovery (entry, failure, delay_millis) VALUES (?1, ?2, ?3)",
                    params![seq, failure, *delay_millis],
                )?;
            }
            SessionEvent::CompactionSkipped {
                request,
                attempt,
                reason,
            } => compaction_outcome(db, seq, kind, Some(*request), Some(*attempt), reason)?,
            SessionEvent::CompactionFailed {
                request,
                attempt,
                error,
            } => {
                if request.is_none() && attempt.is_some() {
                    return Err(rejected("a failed compaction attempt needs its request"));
                }
                compaction_outcome(db, seq, kind, *request, *attempt, error)?;
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
                authorization_scope,
                location,
            } => {
                let origin = origin
                    .as_ref()
                    .map(|origin| {
                        db.query_row(
                            "SELECT c.block FROM tool_call c \
                             JOIN assistant_block b ON b.id = c.block \
                             JOIN assistant_item i ON i.id = b.item \
                             JOIN message_commit m ON m.message = i.message \
                             WHERE m.entry = ?1 AND c.call_id = ?2",
                            params![origin.message, &origin.call_id],
                            |row| Ok(row.get::<i64>(0)?),
                        )?
                        .ok_or_else(|| rejected("job origin names no committed tool call"))
                    })
                    .transpose()?;
                let target = self.target(db, &location.target)?;
                db.execute(
                    "INSERT INTO job (id, created, parent, origin_call, tool, name, role, \
                     arguments, output_schema, accepts_input, background, location_target, \
                     location_workspace, authorization_scope) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    params![
                        job.get(),
                        seq,
                        parent.map(|parent| parent.get()),
                        origin,
                        tool,
                        name.clone(),
                        variant(role)?,
                        json(arguments)?,
                        output_schema.as_ref().map(json).transpose()?,
                        *accepts_input,
                        *background,
                        target,
                        path_bytes(&location.workspace),
                        *authorization_scope,
                    ],
                )?;
                db.execute(
                    "INSERT INTO job_run (job, generation, started) VALUES (?1, 0, ?2)",
                    params![job.get(), seq],
                )?;
            }
            SessionEvent::ApprovalGranted { grant } => {
                db.execute(
                    "INSERT INTO approval_grant (entry, capability, resource, coverage) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        seq,
                        grant.capability.as_str(),
                        json(&grant.resource)?,
                        variant(&grant.coverage)?
                    ],
                )?;
            }
            SessionEvent::ApprovalRevoked { grant } => {
                db.execute(
                    "INSERT INTO approval_revocation (entry, grant_entry) VALUES (?1, ?2)",
                    params![seq, *grant],
                )?;
            }
            SessionEvent::JobStateChanged { job, state } => {
                db.execute(
                    "INSERT INTO job_transition (entry, job, state) VALUES (?1, ?2, ?3)",
                    params![seq, job.get(), variant(state)?],
                )?;
                // A finished job that runs again starts its next generation.
                db.execute(
                    "INSERT INTO job_run (job, generation, started) \
                     SELECT ?1, (SELECT max(generation) + 1 FROM job_run WHERE job = ?1), ?2 \
                     WHERE ?3 = 'running' \
                       AND (SELECT max(entry) FROM job_finish WHERE job = ?1) > coalesce( \
                         (SELECT max(entry) FROM job_transition WHERE job = ?1 AND entry < ?2), 0)",
                    params![job.get(), seq, variant(state)?],
                )?;
            }
            SessionEvent::JobFinished {
                job,
                state,
                error,
                images,
                denial,
            } => {
                // Retained jobs reopen after a running reset; only an interrupted
                // outcome may be followed directly by cancellation.
                let previous = db.query_row(
                    "SELECT f.state, f.entry > coalesce((SELECT max(t.entry) FROM job_transition t \
                     WHERE t.job = f.job AND t.state = 'running'), 0) \
                     FROM job_finish f WHERE f.job = ?1 ORDER BY f.entry DESC LIMIT 1",
                    params![job.get()],
                    |row| Ok((row.get::<String>(0)?, row.get::<bool>(1)?)),
                )?;
                if let Some((previous, current)) = previous
                    && (current || previous == "cancelled")
                {
                    let reopened =
                        current && previous == "interrupted" && variant(state)? == "cancelled";
                    if !reopened {
                        return Err(rejected(format!(
                            "duplicate or invalid terminal event for job {job}"
                        )));
                    }
                }
                let (code, executed) = match denial {
                    Some(denial) => (Some(variant(&denial.code)?), Some(denial.executed)),
                    None => (None, None),
                };
                db.execute(
                    "INSERT INTO job_finish (entry, job, state, error, denial_code, \
                     denial_executed) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        seq,
                        job.get(),
                        variant(state)?,
                        error.clone(),
                        code,
                        executed
                    ],
                )?;
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
        kind: &str,
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

    fn target(&self, db: &Db, name: &str) -> DbResult<i64> {
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

    fn targets(&self, db: &Db, seq: u64, kind: &str, targets: &[TargetDefinition]) -> DbResult<()> {
        for definition in targets {
            let target = self.target(db, &definition.name)?;
            let named = |name: &Option<String>| {
                let name = name.as_deref();
                name.map(|name| self.target(db, name)).transpose()
            };
            let (via, origin) = (named(&definition.via)?, named(&definition.origin)?);
            let ssh = &definition.ssh;
            let key = match &ssh.auth {
                crate::target::TargetAuth::Key { path } => Some(path_bytes(path)),
                _ => None,
            };
            if definition.r#type != crate::target::TargetType::Ssh {
                return Err(rejected("only ssh targets are session definitions"));
            }
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
                    variant(&definition.source)?,
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
             max_output, supports_images, state_mode, digest) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) ON CONFLICT (digest) DO NOTHING",
            params![
                &snapshot.name,
                &profile.provider,
                &profile.model,
                profile.reasoning.clone(),
                profile.max_context,
                profile.max_output,
                profile.supports_images,
                variant(&profile.state_mode)?,
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
            params![
                seq,
                variant(&context.purpose)?,
                profile,
                prompt,
                schema_name,
                schema
            ],
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
        outcome(
            db,
            seq,
            "compaction",
            checkpoint.request,
            checkpoint.attempt,
        )?;
        db.execute(
            "INSERT INTO compaction (entry, schema_version, frontier, message, \
             before_tokens, after_tokens) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                seq,
                checkpoint.schema_version,
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
        todos(db, seq, "compaction", &checkpoint.todos)
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
                "SELECT c.block, c.name FROM tool_call c \
                 JOIN assistant_block b ON b.id = c.block \
                 JOIN assistant_item i ON i.id = b.item \
                 JOIN message_commit m ON m.message = i.message \
                 JOIN entry e ON e.seq = m.entry \
                 WHERE e.agent = ?1 AND c.call_id = ?2 \
                 AND NOT EXISTS (SELECT 1 FROM tool_result r WHERE r.call = c.block) \
                 ORDER BY m.entry DESC LIMIT 1",
                params![agent, &result.call_id],
                |row| Ok((row.get::<i64>(0)?, row.get::<String>(1)?)),
            )?
            .ok_or_else(|| rejected("tool result answers no open committed call"))?;
        if name != result.name {
            return Err(rejected("tool result name differs from its call"));
        }
        let message = db.insert("INSERT INTO message (role) VALUES ('tool')", Vec::new())?;
        tool_result(db, message, call, result)?;
        Ok(message)
    }

    fn message(&self, db: &Db, message: &Message) -> DbResult<i64> {
        match message {
            Message::User(parts) => {
                let id = db.insert("INSERT INTO message (role) VALUES ('user')", Vec::new())?;
                for (position, part) in parts.iter().enumerate() {
                    let (kind, text, attachment) = match part {
                        UserContent::Text { text } => ("text", Some(text), None),
                        UserContent::Runtime { text } => ("runtime", Some(text), None),
                        UserContent::ParentInput { text } => ("parent_input", Some(text), None),
                        UserContent::Compaction { text } => ("compaction", Some(text), None),
                        UserContent::Attachment { attachment } => {
                            ("attachment", None, Some(attachment))
                        }
                    };
                    let (blob, format, file) = match attachment {
                        Some(AttachmentRef::Text(text)) => {
                            (Some(&text.blob), None, text.file.clone())
                        }
                        Some(AttachmentRef::Image(image)) => (
                            Some(&image.blob),
                            Some(variant(&image.format)?),
                            image.file.clone(),
                        ),
                        None => (None, None, None),
                    };
                    let blob = blob.map(|blob| stored_blob(db, blob)).transpose()?;
                    db.execute(
                        "INSERT INTO user_part (message, position, kind, text, blob, \
                         image_format, file) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![id, position, kind, text, blob, format, file],
                    )?;
                }
                Ok(id)
            }
            Message::Assistant(items) => {
                let id = db.insert(
                    "INSERT INTO message (role) VALUES ('assistant')",
                    Vec::new(),
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
    let kind = variant(&item.kind)?;
    let id = db.insert(
        "INSERT INTO assistant_item (message, position, provider_id, kind) \
         VALUES (?1, ?2, ?3, ?4)",
        params![message, item.position, &item.id, kind.clone()],
    )?;
    if let Some(replay) = &item.replay {
        db.execute(
            "INSERT INTO reasoning_replay (item, version, protocol, model, scope, payload, \
             conversation_bound) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                replay.version,
                &replay.protocol,
                &replay.model,
                &replay.scope,
                json(&replay.payload)?,
                replay.conversation_bound
            ],
        )?;
    }
    for block in &item.blocks {
        let text = match &block.content {
            BlockContent::Text { text } | BlockContent::Reasoning { text } => Some(text.clone()),
            BlockContent::ToolCall(_) => None,
        };
        let block_id = db.insert(
            "INSERT INTO assistant_block (item, item_kind, position, provider_id, text) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, kind.clone(), block.position, &block.id, text],
        )?;
        if let BlockContent::ToolCall(call) = &block.content {
            db.execute(
                "INSERT INTO tool_call (block, call_id, name, arguments) VALUES (?1, ?2, ?3, ?4)",
                params![block_id, call.id(), call.name(), json(call.arguments())?],
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
        params![id, position, blob, variant(&image.format)?, image.file.clone()],
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

fn model_attempt(db: &Db, request: u64, attempt: u64) -> DbResult<i64> {
    db.query_row(
        "SELECT entry FROM model_attempt WHERE request = ?1 AND attempt = ?2",
        params![request, attempt],
        |row| Ok(row.get::<i64>(0)?),
    )?
    .ok_or_else(|| rejected(format!("request {request} has no attempt {attempt}")))
}

/// Pin a mode's definition to the entry that first uses it.
fn pin_mode(
    db: &Db,
    seq: u64,
    kind: &str,
    name: &str,
    mode: &crate::tool::policy::Mode,
) -> DbResult<()> {
    db.execute(
        "INSERT INTO mode (entry, kind, name, instructions) VALUES (?1, ?2, ?3, ?4)",
        params![seq, kind, name, mode.instructions.as_deref()],
    )?;
    for capability in &mode.capabilities {
        db.execute(
            "INSERT INTO mode_capability (mode, capability) SELECT id, ?2 FROM mode WHERE name = ?1",
            params![name, capability.as_str()],
        )?;
    }
    Ok(())
}

fn capabilities_at(
    db: &Db,
    seq: u64,
    kind: &str,
    capabilities: &[crate::tool::policy::Capability],
) -> DbResult<()> {
    for capability in capabilities {
        db.execute(
            "INSERT INTO agent_capability (entry, kind, capability) VALUES (?1, ?2, ?3)",
            params![seq, kind, capability.as_str()],
        )?;
    }
    Ok(())
}

fn todos(db: &Db, seq: u64, kind: &str, items: &[crate::agent::TodoItem]) -> DbResult<()> {
    for (position, item) in items.iter().enumerate() {
        db.execute(
            "INSERT INTO todo_item (entry, kind, position, text, status) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![seq, kind, position, &item.text, variant(&item.status)?],
        )?;
    }
    Ok(())
}

fn delivery(
    db: &Db,
    seq: u64,
    kind: &str,
    job: crate::identity::JobId,
    notification: Option<u64>,
    source: Option<u64>,
) -> DbResult<()> {
    db.execute(
        "INSERT INTO job_delivery (entry, kind, job, notification, source) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![seq, kind, job.get(), notification, source],
    )
    .map(drop)
}

/// Record that `attempt` of `request` ended with entry `seq`.
fn outcome(db: &Db, seq: u64, kind: &str, request: u64, attempt: u64) -> DbResult<()> {
    let attempt = model_attempt(db, request, attempt)?;
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
    kind: &str,
    request: Option<u64>,
    attempt: Option<u64>,
    reason: &str,
) -> DbResult<()> {
    let attempt = match (request, attempt) {
        (Some(request), Some(attempt)) => Some(model_attempt(db, request, attempt)?),
        _ => None,
    };
    db.execute(
        "INSERT INTO compaction_outcome (entry, kind, request, attempt, reason) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![seq, kind, request, attempt, reason],
    )
    .map(drop)
}

fn kind(event: &SessionEvent) -> &'static str {
    match event {
        SessionEvent::SessionStarted { .. } => "session_started",
        SessionEvent::TitleSet { .. } => "title_set",
        SessionEvent::TargetsUpserted { .. } => "targets_upserted",
        SessionEvent::AgentStarted { .. } => "agent_started",
        SessionEvent::TodosReplaced { .. } => "todos_replaced",
        SessionEvent::ModelChanged { .. } => "model_selected",
        SessionEvent::ModeChanged { .. } => "mode_changed",
        SessionEvent::MessageCommitted { .. } => "message_committed",
        SessionEvent::Status { .. } => "status",
        SessionEvent::ModelContext { .. } => "model_context",
        SessionEvent::ModelRequested { .. } => "model_requested",
        SessionEvent::Compaction { .. } => "compaction",
        SessionEvent::ModelAttemptStarted { .. } => "model_attempt_started",
        SessionEvent::ModelFailed { .. } => "model_failed",
        SessionEvent::ModelAttemptInterrupted { .. } => "model_attempt_interrupted",
        SessionEvent::ResponseCompleted { .. } => "response_completed",
        SessionEvent::ModelRecoveryScheduled { .. } => "model_recovery_scheduled",
        SessionEvent::CompactionSkipped { .. } => "compaction_skipped",
        SessionEvent::CompactionFailed { .. } => "compaction_failed",
        SessionEvent::Usage { .. } => "usage",
        SessionEvent::JobCreated { .. } => "job_created",
        SessionEvent::ApprovalGranted { .. } => "approval_granted",
        SessionEvent::ApprovalRevoked { .. } => "approval_revoked",
        SessionEvent::JobStateChanged { .. } => "job_state_changed",
        SessionEvent::JobFinished { .. } => "job_finished",
        SessionEvent::JobClaimed { .. } => "job_claimed",
        SessionEvent::JobInjected { .. } => "job_injected",
        SessionEvent::JobMessageDelivered { .. } => "job_message_delivered",
        SessionEvent::AgentCompleted => "agent_completed",
        SessionEvent::AgentInterrupted => "agent_interrupted",
        SessionEvent::AgentFailed { .. } => "agent_failed",
    }
}

#[cfg(test)]
mod tests {
    // Encode -> decode round trip of every session event kind.
    use serde_json::json;

    use crate::{
        execution::ExecutionLocation,
        identity::JobId,
        job::{JobRole, JobState},
        media::{AttachmentRef, ImageFormat, ImageRef, TextRef},
        provider::protocol::{
            AssistantItem, HistoryLifetime, Message, ReplayEnvelope, ResponseSchema, StopReason,
            SystemSegment, ToolCall, ToolDefinition, Usage, UserContent,
        },
        session::{
            CompactionCheckpoint, ModelCallOrigin, ModelContext, ModelFailureKind, ModelPurpose,
            SessionEvent,
            db::tests::{Fixture, result, user},
            fixture::{child_started, profile},
        },
    };

    #[test]
    fn request_history_may_leave_out_projected_sources() {
        let mut fixture = Fixture::new();
        let root = fixture.start("/workspace");
        let context = fixture.one(
            root.clone(),
            SessionEvent::ModelContext {
                context: ModelContext {
                    purpose: ModelPurpose::Agent,
                    profile: profile(),
                    system: Vec::new(),
                    tools: Vec::new(),
                    response_schema: None,
                },
            },
        );
        let mut commit = |text| {
            let message = user(text);
            fixture.one(root.clone(), SessionEvent::MessageCommitted { message })
        };
        let (first, _skipped, last) = (commit("first"), commit("skipped"), commit("last"));
        for history in [vec![first, last], vec![last], Vec::new()] {
            fixture.one(
                root.clone(),
                SessionEvent::ModelRequested {
                    context,
                    history,
                    tail: Vec::new(),
                    history_lifetime: HistoryLifetime::Ending,
                    purpose: ModelPurpose::Agent,
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
        // History is the agent's projected history; nothing else can be named.
        fixture.reject(
            root.clone(),
            SessionEvent::ModelRequested {
                context,
                history: vec![context],
                tail: Vec::new(),
                history_lifetime: HistoryLifetime::Ending,
                purpose: ModelPurpose::Agent,
            },
        );
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
        // An attempt ends once.
        let interrupted = SessionEvent::ModelAttemptInterrupted {
            request,
            attempt: 1,
        };
        fixture.reject(root.clone(), interrupted);
        one!(SessionEvent::ModelRecoveryScheduled {
            request,
            attempt: 2,
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
        let resource = crate::tool::policy::ResourceId::mcp("server", "tool");
        let grant = one!(SessionEvent::ApprovalGranted {
            grant: crate::tool::policy::ApprovalGrant::descendants(
                crate::tool::policy::Capability::Mcp,
                resource,
            ),
        });
        one!(SessionEvent::ApprovalRevoked { grant });
        one!(SessionEvent::JobClaimed { job });
        one!(SessionEvent::JobInjected { job });
        one!(SessionEvent::JobMessageDelivered {
            job,
            source: assistant,
            notification: prompt,
        });
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

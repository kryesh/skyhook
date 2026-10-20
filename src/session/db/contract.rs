//! Targets and the journaled contract: agent starts, modes, profiles and model contexts.

use std::collections::HashMap;

use super::{decode::*, encode::*, *};
use crate::{
    execution::{path_bytes, path_from_bytes},
    provider::{
        profile::ModelProfile,
        protocol::{ResponseSchema, SystemSegment, ToolDefinition},
    },
    session::{ModeSelection, ModelContext, ProfileSnapshot, SessionEvent},
    target::{SshAuth, SshOptions, TargetAuth, TargetDefinition, TargetName},
    tool::policy::{Capability, Mode},
};

fn digest(value: &impl serde::Serialize) -> DbResult<Vec<u8>> {
    use sha2::Digest as _;
    let bytes = serde_json::to_vec(value).map_err(|error| corrupt(error.to_string()))?;
    Ok(sha2::Sha256::digest(bytes).to_vec())
}

fn by_digest(db: &Db, table: &str, digest: Vec<u8>) -> DbResult<i64> {
    db.query_row(
        format!("SELECT id FROM {table} WHERE digest = ?1"),
        params![digest],
        |row| Ok(row.get::<i64>(0)?),
    )?
    .ok_or_else(|| corrupt(format!("{table} row is missing")))
}

impl Encoder {
    pub(super) fn contract(&mut self, db: &Db, entry: Entry, event: &SessionEvent) -> DbResult<()> {
        let seq = entry.seq;
        match event {
            SessionEvent::SessionStarted { targets, .. }
            | SessionEvent::TargetsUpserted { targets } => self.targets(db, seq, targets),
            SessionEvent::AgentStarted {
                profile,
                mode,
                capabilities,
                location,
                ..
            } => {
                let profile = self::profile(db, profile)?;
                let target = self.target(db, &location.target)?;
                db.execute(
                    "INSERT INTO agent_start (entry, profile, location_target, \
                     location_workspace) VALUES (?1, ?2, ?3, ?4)",
                    params![seq, profile, target, path_bytes(&location.workspace)],
                )?;
                if let Some(mode) = mode {
                    self::mode(db, seq, mode)?;
                }
                self::capabilities(db, seq, capabilities)
            }
            SessionEvent::ModeChanged { mode, capabilities } => {
                self::mode(db, seq, mode)?;
                self::capabilities(db, seq, capabilities)
            }
            SessionEvent::ModelChanged { profile } => {
                let profile = self::profile(db, profile)?;
                db.execute(
                    "INSERT INTO model_selection (entry, profile) VALUES (?1, ?2)",
                    params![seq, profile],
                )
                .map(drop)
            }
            SessionEvent::ModelContext { context } => self::context(db, seq, context),
            _ => unreachable!("{} is not a contract event", event.kind()),
        }
    }

    fn targets(&mut self, db: &Db, entry: u64, targets: &[TargetDefinition]) -> DbResult<()> {
        for definition in targets {
            let target = self.target(db, &definition.name.clone().into())?;
            let mut named = |name: &Option<TargetName>| {
                (name.as_ref())
                    .map(|name| self.target(db, &name.clone().into()))
                    .transpose()
            };
            let (via, origin) = (named(&definition.via)?, named(&definition.origin)?);
            let ssh = &definition.ssh;
            let key = match &ssh.auth {
                TargetAuth::Key { path } => Some(path_bytes(path)),
                _ => None,
            };
            let revision = db.insert(
                "INSERT INTO target_revision (target, entry, revision, source, host, \
                 workspace, ssh_user, ssh_port, ssh_auth, ssh_key_path, ssh_external_agent, \
                 via, origin) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    target,
                    entry,
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
}

/// Apply a mode: an entry with a definition records the name's next edition, and
/// every entry names the latest one.
fn mode(db: &Db, entry: u64, mode: &ModeSelection) -> DbResult<()> {
    let name = &mode.name;
    if let Some(definition) = &mode.definition {
        db.execute(
            "INSERT INTO mode (name, version, entry, instructions, hint) \
             SELECT ?1, coalesce(max(version), 0) + 1, ?2, ?3, ?4 FROM mode WHERE name = ?1",
            params![
                name,
                entry,
                definition.instructions.as_ref(),
                definition.hint.as_ref()
            ],
        )?;
        for capability in &definition.capabilities {
            db.execute(
                "INSERT INTO mode_capability (mode_name, mode_version, capability) \
                 SELECT name, version, ?2 FROM mode WHERE entry = ?1",
                params![entry, *capability],
            )?;
        }
    }
    let inserted = db.execute(
        "INSERT INTO agent_mode (entry, mode_name, mode_version) \
         SELECT ?1, name, max(version) FROM mode WHERE name = ?2 GROUP BY name",
        params![entry, name],
    )?;
    if inserted == 0 {
        return Err(rejected(format!(
            "mode {name:?} has no edition recorded in the session"
        )));
    }
    Ok(())
}

fn capabilities(db: &Db, entry: u64, capabilities: &[Capability]) -> DbResult<()> {
    for capability in capabilities {
        db.execute(
            "INSERT INTO agent_capability (entry, capability) VALUES (?1, ?2)",
            params![entry, *capability],
        )?;
    }
    Ok(())
}

fn profile(db: &Db, snapshot: &ProfileSnapshot) -> DbResult<i64> {
    let digest = digest(snapshot)?;
    let profile = &snapshot.profile;
    db.execute(
        "INSERT INTO model_profile (name, provider, model, reasoning, \
         max_context, max_output, supports_images, state_mode, hint, digest) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) ON CONFLICT (digest) DO NOTHING",
        params![
            &snapshot.name.model,
            &snapshot.name.provider,
            &profile.model,
            profile.reasoning.as_deref(),
            profile.max_context.get(),
            profile.max_output.get(),
            profile.supports_images,
            profile.state_mode,
            profile.hint.as_ref(),
            digest.clone(),
        ],
    )?;
    by_digest(db, "model_profile", digest)
}

fn context(db: &Db, seq: u64, context: &ModelContext) -> DbResult<()> {
    let profile = profile(db, &context.profile)?;
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
        Some(schema) => (Some(schema.name().to_owned()), Some(json(schema.schema())?)),
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

pub(super) fn profiles(db: &Db) -> DbResult<HashMap<i64, ProfileSnapshot>> {
    let limit =
        |tokens: u64| std::num::NonZeroU64::new(tokens).ok_or_else(|| corrupt("zero model limit"));
    keyed(
        db,
        "SELECT id, name, provider, model, reasoning, max_context, max_output, \
         supports_images, state_mode, hint FROM model_profile",
        |row| {
            Ok(ProfileSnapshot {
                name: model_ref(&row.get::<String>(2)?, &row.get::<String>(1)?)?,
                profile: ModelProfile {
                    model: parsed(row.get(3)?)?,
                    reasoning: row.get(4)?,
                    max_context: limit(row.get(5)?)?,
                    max_output: limit(row.get(6)?)?,
                    supports_images: row.get(7)?,
                    state_mode: enum_column(row, 8)?,
                    hint: row.get::<Option<String>>(9)?.map(parsed).transpose()?,
                },
            })
        },
    )
}

/// The target revisions each entry in scope journaled.
pub(super) fn targets(events: &Events) -> DbResult<HashMap<i64, Vec<TargetDefinition>>> {
    let db = events.db;
    let options = grouped(
        db,
        events.scoped(
            "SELECT revision, key, value FROM target_ssh_option \
             WHERE revision IN (SELECT id FROM target_revision {scope}) ORDER BY revision, key",
            "entry",
        )?,
        |row| Ok((row.get::<String>(1)?, row.get::<String>(2)?)),
    )?;
    let mut targets: HashMap<i64, Vec<TargetDefinition>> = HashMap::new();
    let Query { sql, params } = events.scoped(
        "SELECT r.entry, r.id, t.name, r.revision, r.source, r.host, r.workspace, r.ssh_user, \
         r.ssh_port, r.ssh_auth, r.ssh_key_path, r.ssh_external_agent, v.name, o.name \
         FROM target_revision r JOIN target t ON t.id = r.target \
         LEFT JOIN target v ON v.id = r.via LEFT JOIN target o ON o.id = r.origin \
         {scope} ORDER BY r.entry, r.id",
        "r.entry",
    )?;
    for (entry, target) in db.query(sql, params, |row| {
        let auth = match (enum_column(row, 9)?, row.get::<Option<Vec<u8>>>(10)?) {
            (SshAuth::Default, None) => TargetAuth::Default,
            (SshAuth::Agent, None) => TargetAuth::Agent,
            (SshAuth::Key, Some(key)) => TargetAuth::Key {
                path: path_from_bytes(key),
            },
            _ => return Err(corrupt("target authentication is inconsistent")),
        };
        let port = row.get::<Option<u32>>(8)?;
        Ok((
            row.get::<i64>(0)?,
            TargetDefinition {
                name: parsed(row.get(2)?)?,
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
                workspace: path_from_bytes(row.get(6)?),
                via: row.get::<Option<String>>(12)?.map(parsed).transpose()?,
                origin: row.get::<Option<String>>(13)?.map(parsed).transpose()?,
                source: enum_column(row, 4)?,
                revision: row.get(3)?,
            },
        ))
    })? {
        targets.entry(entry).or_default().push(target);
    }
    Ok(targets)
}

/// Agent starts, mode and model changes, and model contexts.
pub(super) fn events(events: &mut Events) -> DbResult<()> {
    let db = events.db;
    let profiles = profiles(db)?;
    let profile = |id: i64| {
        profiles
            .get(&id)
            .cloned()
            .ok_or_else(|| corrupt("model profile is missing"))
    };
    let mut capabilities = grouped(
        db,
        events.scoped(
            "SELECT entry, capability FROM agent_capability {scope}",
            "entry",
        )?,
        |row| enum_column(row, 1),
    )?;
    // Mode editions by the entry that recorded them.
    let mode_capabilities = grouped(
        db,
        events.scoped(
            "SELECT m.entry, c.capability FROM mode_capability c \
             JOIN mode m ON m.name = c.mode_name AND m.version = c.mode_version {scope}",
            "m.entry",
        )?,
        |row| enum_column(row, 1),
    )?;
    let editions = events.scoped(
        "SELECT entry, instructions, hint FROM mode {scope}",
        "entry",
    )?;
    let mut editions = keyed(db, editions, |row| {
        let capabilities = mode_capabilities.get(&row.get::<i64>(0)?);
        Ok(Mode {
            capabilities: sorted(capabilities.cloned().unwrap_or_default()),
            instructions: row.get::<Option<String>>(1)?.map(parsed).transpose()?,
            hint: row.get::<Option<String>>(2)?.map(parsed).transpose()?,
        })
    })?;
    let modes = events.scoped("SELECT entry, mode_name FROM agent_mode {scope}", "entry")?;
    let mut modes = keyed(db, modes, |row| parsed(row.get(1)?))?;
    events.load(
        "SELECT s.entry, a.owner_job, a.available_depth, t.name, s.location_workspace, \
         s.profile FROM agent_start s \
         JOIN entry e ON e.seq = s.entry JOIN agent a ON a.id = e.agent \
         JOIN target t ON t.id = s.location_target {scope}",
        "s.entry",
        |row| {
            let entry = row.get(0)?;
            Ok(SessionEvent::AgentStarted {
                owner_job: row.get::<Option<i64>>(1)?.map(job).transpose()?,
                profile: profile(row.get(5)?)?,
                available_depth: row.get(2)?,
                mode: (modes.remove(&entry)).map(|name| ModeSelection {
                    name,
                    definition: editions.remove(&entry),
                }),
                capabilities: sorted(take(&mut capabilities, entry)),
                location: location(row.get(3)?, row.get(4)?)?,
            })
        },
    )?;
    // Every other entry that applied a mode changed it.
    for (entry, name) in modes {
        let definition = editions.remove(&entry);
        let mode = ModeSelection { name, definition };
        let capabilities = sorted(take(&mut capabilities, entry));
        events.insert(entry, SessionEvent::ModeChanged { mode, capabilities })?;
    }
    if !editions.is_empty() || !capabilities.is_empty() {
        return Err(corrupt("mode rows belong to an entry that applied no mode"));
    }
    let selections = "SELECT entry, profile FROM model_selection {scope}";
    events.load(selections, "entry", |row| {
        Ok(SessionEvent::ModelChanged {
            profile: profile(row.get(1)?)?,
        })
    })?;
    let prompts = grouped(
        db,
        events.scoped(
            "SELECT prompt, text, cache FROM system_segment \
             WHERE prompt IN (SELECT c.system_prompt FROM model_context c {scope}) \
             ORDER BY prompt, position",
            "c.entry",
        )?,
        |row| {
            Ok(SystemSegment {
                text: row.get(1)?,
                cache: row.get(2)?,
            })
        },
    )?;
    let tools = grouped(
        db,
        events.scoped(
            "SELECT t.context, d.name, d.description, d.input_schema FROM model_context_tool t \
             JOIN tool_definition d ON d.id = t.tool \
             WHERE t.context IN (SELECT c.entry FROM model_context c {scope}) \
             ORDER BY t.context, t.position",
            "c.entry",
        )?,
        |row| {
            Ok(ToolDefinition {
                name: row.get(1)?,
                description: row.get(2)?,
                input_schema: parse_json(&row.get::<String>(3)?)?,
            })
        },
    )?;
    events.load(
        "SELECT c.entry, c.purpose, c.profile, c.system_prompt, c.response_schema_name, \
         c.response_schema FROM model_context c {scope}",
        "c.entry",
        |row| {
            let response_schema =
                match (row.get::<Option<String>>(4)?, row.get::<Option<String>>(5)?) {
                    (Some(name), Some(schema)) => Some(
                        ResponseSchema::new(name, parse_json(&schema)?)
                            .map_err(|error| corrupt(error.to_string()))?,
                    ),
                    _ => None,
                };
            Ok(SessionEvent::ModelContext {
                context: ModelContext {
                    purpose: enum_column(row, 1)?,
                    profile: profile(row.get(2)?)?,
                    system: (prompts.get(&row.get::<i64>(3)?).cloned()).unwrap_or_default(),
                    tools: tools.get(&row.get::<i64>(0)?).cloned().unwrap_or_default(),
                    response_schema,
                },
            })
        },
    )
}

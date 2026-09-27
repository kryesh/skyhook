//! Targets and the pinned contract: agent starts, modes, profiles and model contexts.

use std::collections::HashMap;

use super::{decode::*, encode::*, *};
use crate::{
    execution::{path_bytes, path_from_bytes},
    provider::{
        profile::ModelProfile,
        protocol::{ResponseSchema, SystemSegment, ToolDefinition},
    },
    session::{EntryKind, ModeSelection, ModelContext, ProfileSnapshot, SessionEvent},
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
        &format!("SELECT id FROM {table} WHERE digest = ?1"),
        params![digest],
        |row| Ok(row.get::<i64>(0)?),
    )?
    .ok_or_else(|| corrupt(format!("{table} row is missing")))
}

impl Encoder {
    pub(super) fn contract(&mut self, db: &Db, entry: Entry, event: &SessionEvent) -> DbResult<()> {
        let Entry { seq, kind, .. } = entry;
        match event {
            SessionEvent::SessionStarted { targets, .. }
            | SessionEvent::TargetsUpserted { targets } => self.targets(db, entry, targets),
            SessionEvent::AgentStarted {
                profile,
                mode,
                capabilities,
                location,
                ..
            } => {
                let profile = profile
                    .as_ref()
                    .map(|profile| self::profile(db, profile))
                    .transpose()?;
                let target = self.target(db, &location.target)?;
                db.execute(
                    "INSERT INTO agent_start (entry, profile, location_target, \
                     location_workspace) VALUES (?1, ?2, ?3, ?4)",
                    params![seq, profile, target, path_bytes(&location.workspace)],
                )?;
                if let Some(mode) = mode {
                    self::mode(db, entry, mode)?;
                }
                self::capabilities(db, entry, capabilities)
            }
            SessionEvent::ModeChanged { mode, capabilities } => {
                self::mode(db, entry, mode)?;
                self::capabilities(db, entry, capabilities)
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
            _ => unreachable!("{kind} is not a contract event"),
        }
    }

    fn targets(&mut self, db: &Db, entry: Entry, targets: &[TargetDefinition]) -> DbResult<()> {
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
                "INSERT INTO target_revision (target, entry, kind, revision, source, host, \
                 workspace, ssh_user, ssh_port, ssh_auth, ssh_key_path, ssh_external_agent, \
                 via, origin) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    target,
                    entry.seq,
                    entry.kind,
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

/// Apply a mode, pinning its definition to the entry that first uses it.
fn mode(db: &Db, entry: Entry, mode: &ModeSelection) -> DbResult<()> {
    let name = &mode.name;
    if let Some(definition) = &mode.definition {
        db.execute(
            "INSERT INTO mode (entry, kind, name, instructions, hint) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                entry.seq,
                entry.kind,
                name,
                definition.instructions.as_ref(),
                definition.hint.as_ref()
            ],
        )?;
        for capability in &definition.capabilities {
            db.execute(
                "INSERT INTO mode_capability (mode, capability) \
                 SELECT id, ?2 FROM mode WHERE name = ?1",
                params![name, *capability],
            )?;
        }
    }
    let inserted = db.execute(
        "INSERT INTO agent_mode (entry, kind, mode) SELECT ?1, ?2, id FROM mode WHERE name = ?3",
        params![entry.seq, entry.kind, name],
    )?;
    if inserted == 0 {
        return Err(rejected(format!(
            "mode {name:?} is not pinned by the session"
        )));
    }
    Ok(())
}

fn capabilities(db: &Db, entry: Entry, capabilities: &[Capability]) -> DbResult<()> {
    for capability in capabilities {
        db.execute(
            "INSERT INTO agent_capability (entry, kind, capability) VALUES (?1, ?2, ?3)",
            params![entry.seq, entry.kind, *capability],
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

fn profiles(db: &Db) -> DbResult<HashMap<i64, ProfileSnapshot>> {
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

/// The target revisions each entry journaled.
pub(super) fn targets(db: &Db) -> DbResult<HashMap<i64, Vec<TargetDefinition>>> {
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
        },
    )? {
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
    let agent_capabilities = grouped(
        db,
        "SELECT entry, capability FROM agent_capability",
        |row| enum_column(row, 1),
    )?;
    let capabilities_at = |entry: i64| {
        let capabilities = agent_capabilities.get(&entry);
        sorted(capabilities.cloned().unwrap_or_default())
    };
    let mode_capabilities = grouped(db, "SELECT mode, capability FROM mode_capability", |row| {
        enum_column(row, 1)
    })?;
    // Mode definitions by the entry that pinned them.
    let pinned_modes = keyed(
        db,
        "SELECT entry, id, instructions, hint FROM mode",
        |row| {
            let capabilities = mode_capabilities.get(&row.get::<i64>(1)?);
            Ok(Mode {
                capabilities: sorted(capabilities.cloned().unwrap_or_default()),
                instructions: row.get::<Option<String>>(2)?.map(parsed).transpose()?,
                hint: row.get::<Option<String>>(3)?.map(parsed).transpose()?,
            })
        },
    )?;
    let selection = |entry: i64, name: Option<String>| {
        let definition = pinned_modes.get(&entry).cloned();
        let name = name.map(parsed).transpose()?;
        DbResult::Ok(name.map(|name| ModeSelection { name, definition }))
    };
    events.load(
        "SELECT s.entry, a.owner_job, a.available_depth, t.name, s.location_workspace, \
         s.profile, m.name FROM agent_start s \
         JOIN entry e ON e.seq = s.entry JOIN agent a ON a.id = e.agent \
         JOIN target t ON t.id = s.location_target \
         LEFT JOIN agent_mode am ON am.entry = s.entry LEFT JOIN mode m ON m.id = am.mode",
        |row| {
            Ok(SessionEvent::AgentStarted {
                owner_job: row.get::<Option<i64>>(1)?.map(job).transpose()?,
                profile: row.get::<Option<i64>>(5)?.map(profile).transpose()?,
                available_depth: row.get(2)?,
                mode: selection(row.get(0)?, row.get(6)?)?,
                capabilities: capabilities_at(row.get(0)?),
                location: location(row.get(3)?, row.get(4)?)?,
            })
        },
    )?;
    events.load_with(
        "SELECT am.entry, m.name FROM agent_mode am JOIN mode m ON m.id = am.mode \
         WHERE am.kind = ?1",
        params![EntryKind::ModeChanged],
        |row| {
            Ok(SessionEvent::ModeChanged {
                mode: selection(row.get(0)?, row.get(1)?)?
                    .ok_or_else(|| corrupt("mode_changed entry has no mode"))?,
                capabilities: capabilities_at(row.get(0)?),
            })
        },
    )?;
    events.load("SELECT entry, profile FROM model_selection", |row| {
        Ok(SessionEvent::ModelChanged {
            profile: profile(row.get(1)?)?,
        })
    })?;
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
    events.load(
        "SELECT entry, purpose, profile, system_prompt, response_schema_name, response_schema \
         FROM model_context",
        |row| {
            let response_schema = match (row.get(4)?, row.get::<Option<String>>(5)?) {
                (Some(name), Some(schema)) => Some(ResponseSchema {
                    name,
                    schema: parse_json(&schema)?,
                }),
                _ => None,
            };
            Ok(SessionEvent::ModelContext {
                context: ModelContext {
                    purpose: enum_column(row, 1)?,
                    profile: profile(row.get(2)?)?,
                    system: prompts
                        .get(&row.get::<i64>(3)?)
                        .cloned()
                        .unwrap_or_default(),
                    tools: tools.get(&row.get::<i64>(0)?).cloned().unwrap_or_default(),
                    response_schema,
                },
            })
        },
    )
}

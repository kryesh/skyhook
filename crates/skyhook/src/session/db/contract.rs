//! Targets and the journaled contract: agent starts, modes, profiles and model contexts.

use std::collections::HashMap;

use super::{decode::*, encode::*, *};
use crate::target::{
    Connection, ConnectionKind, Password, SshAuth, SshAuthKind, SshOptions, TargetDefinition,
    WinrmOptions,
};
use crate::tool::catalog::ToolDefinition;
use crate::tool::policy::CapabilitySet;
use crate::{
    config::mode::Edition,
    provider::{
        model::ModelProfile,
        protocol::{ResponseSchema, SystemSegment},
    },
    session::{ModeSelection, ModelContext, ProfileSnapshot, SessionEvent},
    tool::policy::InteractiveListed,
};
use skyhook_tools::target::TargetName;
use skyhook_worker::{
    op::TargetPath,
    origin::{
        ssh::SshUser,
        winrm::{Auth, AuthKind, Fingerprint, Tls, TlsKind},
    },
    secret::Credential,
};

fn digest(value: &impl serde::Serialize) -> DbResult<Vec<u8>> {
    use sha2::Digest as _;
    let bytes = serde_json::to_vec(value).map_err(|error| corrupt(error.to_string()))?;
    Ok(sha2::Sha256::digest(bytes).to_vec())
}

fn known(db: &Db, table: &str, digest: Vec<u8>) -> DbResult<Option<i64>> {
    db.query_row(
        format!("SELECT id FROM {table} WHERE digest = ?1"),
        params![digest],
        |row| Ok(row.get::<i64>(0)?),
    )
}

fn by_digest(db: &Db, table: &str, digest: Vec<u8>) -> DbResult<i64> {
    known(db, table, digest)?.ok_or_else(|| corrupt(format!("{table} row is missing")))
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
                    params![seq, profile, target, location.workspace.as_str()],
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
            SessionEvent::ModelContext { context } => self.context(db, seq, context),
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
            let revision = db.insert(
                "INSERT INTO target_revision (target, entry, revision, source, host, os, \
                 workspace, transport, via, origin) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    target,
                    entry,
                    definition.revision,
                    definition.source,
                    &definition.host,
                    definition.os,
                    definition.workspace.as_str(),
                    definition.connection.kind(),
                    via,
                    origin,
                ],
            )?;
            match &definition.connection {
                Connection::Ssh(ssh) => self::ssh(db, revision, ssh)?,
                Connection::Winrm(winrm) => self::winrm(db, revision, winrm)?,
            }
            if let Some(password) = definition.connection.password() {
                self::password(db, revision, password)?;
            }
        }
        Ok(())
    }
}

fn ssh(db: &Db, revision: i64, ssh: &SshOptions) -> DbResult<()> {
    let key = match &ssh.auth {
        SshAuth::Key { path, .. } => Some(path.as_str()),
        _ => None,
    };
    db.execute(
        "INSERT INTO target_ssh (revision, user, port, auth, key_path, forward_agent) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            revision,
            ssh.user.as_ref().map(SshUser::as_str),
            ssh.port.map(std::num::NonZeroU16::get),
            ssh.auth.kind(),
            key,
            ssh.forward_agent,
        ],
    )?;
    for (key, value) in &ssh.options {
        db.execute(
            "INSERT INTO target_ssh_option (revision, key, value) VALUES (?1, ?2, ?3)",
            params![revision, key.as_str(), value.as_str()],
        )?;
    }
    Ok(())
}

fn winrm(db: &Db, revision: i64, winrm: &WinrmOptions) -> DbResult<()> {
    let (ca, fingerprint) = match &winrm.tls {
        Tls::Ca(path) => (Some(path.as_str()), None),
        Tls::Fingerprint(fingerprint) => (None, Some(fingerprint.as_bytes().to_vec())),
        Tls::System | Tls::Insecure | Tls::Off => (None, None),
    };
    db.execute(
        "INSERT INTO target_winrm (revision, user, auth, kdc, tls, ca_path, fingerprint, port) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            revision,
            winrm.auth.user(),
            winrm.auth.kind(),
            winrm.auth.kdc().map(ToString::to_string),
            winrm.tls.kind(),
            ca,
            fingerprint,
            winrm.port.map(std::num::NonZeroU16::get),
        ],
    )
    .map(drop)
}

/// Journal the password a revision reads: a credential its definition gives, as given,
/// and of the configuration's only that the revision reads it.
fn password(db: &Db, revision: i64, password: &Password) -> DbResult<()> {
    let (kind, text, file, argv) = match password.given().map(Credential::parts) {
        Some((kind, text, file, argv)) => (Some(kind), text, file, argv),
        None => (None, None, None, &[][..]),
    };
    db.execute(
        "INSERT INTO target_password (revision, source, text, file) VALUES (?1, ?2, ?3, ?4)",
        params![revision, kind, text, file.map(TargetPath::as_str)],
    )?;
    for (position, value) in argv.iter().enumerate() {
        db.execute(
            "INSERT INTO target_password_argument (revision, position, value) VALUES (?1, ?2, ?3)",
            params![revision, position as i64, value],
        )?;
    }
    Ok(())
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
        for capability in definition.capabilities.iter() {
            codes::record_capability(db, capability)?;
            db.execute(
                "INSERT INTO mode_capability (mode_name, mode_version, capability) \
                 SELECT name, version, ?2 FROM mode WHERE entry = ?1",
                params![entry, capability],
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

fn capabilities(db: &Db, entry: u64, capabilities: &CapabilitySet) -> DbResult<()> {
    for capability in capabilities.iter() {
        codes::record_capability(db, capability)?;
        db.execute(
            "INSERT INTO agent_capability (entry, capability) VALUES (?1, ?2)",
            params![entry, capability],
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
            profile.reasoning,
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

/// A tool definition by its allocation, which it keeps: the surface that generated it
/// shares it with every context it opens.
pub(super) struct Shared(std::sync::Arc<ToolDefinition>);

impl PartialEq for Shared {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Shared {}

impl std::hash::Hash for Shared {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::sync::Arc::as_ptr(&self.0).hash(state);
    }
}

impl Encoder {
    fn context(&mut self, db: &Db, seq: u64, context: &ModelContext) -> DbResult<()> {
        self::context(db, seq, context)?;
        for (position, tool) in context.tools.iter().enumerate() {
            let shared = Shared(tool.clone());
            let tool = match self.tools.get(&shared) {
                Some(tool) => *tool,
                None => {
                    let id = self::tool(db, tool)?;
                    *self.tools.entry(shared).or_insert(id)
                }
            };
            db.execute(
                "INSERT INTO model_context_tool (context, position, tool) VALUES (?1, ?2, ?3)",
                params![seq, position, tool],
            )?;
        }
        Ok(())
    }
}

/// The row of `tool`, inserted unless a context recorded it before.
fn tool(db: &Db, tool: &ToolDefinition) -> DbResult<i64> {
    let digest = digest(tool)?;
    let known = db.query_row(
        "SELECT id FROM tool_definition WHERE digest = ?1",
        params![digest.clone()],
        |row| Ok(row.get::<i64>(0)?),
    )?;
    // Most contexts repeat known tools: only a new one's schema is serialized.
    match known {
        Some(tool) => Ok(tool),
        None => db.insert(
            "INSERT INTO tool_definition (name, description, input_schema, digest) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                &tool.name,
                &tool.description,
                json(&tool.input_schema)?,
                digest
            ],
        ),
    }
}

/// `context`'s own rows, but for its tools.
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
    )
    .map(drop)
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
                    reasoning: optional_enum_column(row, 4)?,
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
        |row| {
            let (key, value) = (row.get::<String>(1)?.parse(), row.get::<String>(2)?.parse());
            key.ok()
                .zip(value.ok())
                .ok_or_else(|| corrupt("SSH option is invalid"))
        },
    )?;
    let arguments = grouped(
        db,
        events.scoped(
            "SELECT revision, value FROM target_password_argument \
             WHERE revision IN (SELECT id FROM target_revision {scope}) ORDER BY revision, position",
            "entry",
        )?,
        |row| Ok(row.get::<String>(1)?),
    )?;
    let passwords = keyed(
        db,
        events.scoped(
            "SELECT revision, source, text, file FROM target_password \
             WHERE revision IN (SELECT id FROM target_revision {scope})",
            "entry",
        )?,
        |row| {
            let Some(kind) = optional_enum_column(row, 1)? else {
                return Ok(Password::Configured);
            };
            let file = row.get::<Option<String>>(3)?.map(TargetPath::from);
            let argv = (arguments.get(&row.get::<i64>(0)?).cloned()).unwrap_or_default();
            (Credential::from_parts(kind, row.get(2)?, file, argv).map(Password::Given))
                .ok_or_else(|| corrupt("target password is inconsistent"))
        },
    )?;
    let port = |row: &Row, index| -> DbResult<_> {
        let port = row.get::<Option<u32>>(index)?;
        Ok(port
            .and_then(|port| u16::try_from(port).ok())
            .and_then(std::num::NonZeroU16::new))
    };
    let mut ssh = keyed(
        db,
        events.scoped(
            "SELECT revision, user, port, auth, key_path, forward_agent FROM target_ssh \
             WHERE revision IN (SELECT id FROM target_revision {scope})",
            "entry",
        )?,
        |row| {
            let revision = row.get::<i64>(0)?;
            let password = passwords.get(&revision).cloned();
            let key = row.get::<Option<String>>(4)?;
            let auth = match (enum_column(row, 3)?, key, password) {
                (SshAuthKind::Agent, None, None) => SshAuth::Agent,
                (SshAuthKind::Key, Some(key), password) => SshAuth::Key {
                    path: key.into(),
                    password,
                },
                (SshAuthKind::Password, None, Some(password)) => SshAuth::Password { password },
                _ => return Err(corrupt("target authentication is inconsistent")),
            };
            Ok(SshOptions {
                user: (row.get::<Option<String>>(1)?)
                    .map(|user| user.parse().map_err(|_| corrupt("SSH user is invalid")))
                    .transpose()?,
                port: port(row, 2)?,
                auth,
                forward_agent: row.get(5)?,
                options: (options.get(&revision).into_iter().flatten().cloned()).collect(),
            })
        },
    )?;
    let mut winrm = keyed(
        db,
        events.scoped(
            "SELECT revision, user, auth, kdc, tls, ca_path, fingerprint, port FROM target_winrm \
             WHERE revision IN (SELECT id FROM target_revision {scope})",
            "entry",
        )?,
        |row| {
            let inconsistent = || corrupt("target authentication is inconsistent");
            let password =
                (passwords.get(&row.get::<i64>(0)?).cloned()).ok_or_else(inconsistent)?;
            let user = row.get::<String>(1)?;
            let kdc = row.get::<Option<String>>(3)?.map(parsed).transpose()?;
            let auth = match (enum_column(row, 2)?, kdc) {
                (AuthKind::Kerberos, kdc) => Auth::Kerberos {
                    principal: parsed(user)?,
                    password,
                    kdc,
                },
                (AuthKind::Negotiate, None) => Auth::Negotiate { user, password },
                (AuthKind::Basic, None) => Auth::Basic { user, password },
                _ => return Err(inconsistent()),
            };
            let (ca, fingerprint) = (row.get::<Option<String>>(5)?, row.get(6)?);
            let tls = match (enum_column(row, 4)?, ca, fingerprint) {
                (TlsKind::System, None, None) => Tls::System,
                (TlsKind::Insecure, None, None) => Tls::Insecure,
                (TlsKind::Off, None, None) => Tls::Off,
                (TlsKind::Ca, Some(path), None) => Tls::Ca(path.into()),
                (TlsKind::Fingerprint, None, Some(fingerprint)) => {
                    Tls::Fingerprint(Fingerprint::from_bytes(bytes(fingerprint)?))
                }
                _ => return Err(corrupt("target TLS is inconsistent")),
            };
            let port = port(row, 7)?;
            Ok(WinrmOptions { auth, tls, port })
        },
    )?;
    let mut targets: HashMap<i64, Vec<TargetDefinition>> = HashMap::new();
    let Query { sql, params } = events.scoped(
        "SELECT r.entry, r.id, t.name, r.revision, r.source, r.host, r.workspace, v.name, \
         o.name, r.os, r.transport \
         FROM target_revision r JOIN target t ON t.id = r.target \
         LEFT JOIN target v ON v.id = r.via LEFT JOIN target o ON o.id = r.origin \
         {scope} ORDER BY r.entry, r.id",
        "r.entry",
    )?;
    for (entry, target) in db.query(sql, params, |row| {
        let revision = row.get::<i64>(1)?;
        let (ssh, winrm) = (ssh.remove(&revision), winrm.remove(&revision));
        let connection = match (enum_column(row, 10)?, ssh, winrm) {
            (ConnectionKind::Ssh, Some(ssh), None) => Connection::Ssh(ssh),
            (ConnectionKind::Winrm, None, Some(winrm)) => Connection::Winrm(winrm),
            _ => return Err(corrupt("target settings are not its transport's")),
        };
        Ok((
            row.get::<i64>(0)?,
            TargetDefinition {
                name: parsed(row.get(2)?)?,
                host: row.get(5)?,
                os: enum_column(row, 9)?,
                connection,
                workspace: row.get::<String>(6)?.into(),
                via: row.get::<Option<String>>(7)?.map(parsed).transpose()?,
                origin: row.get::<Option<String>>(8)?.map(parsed).transpose()?,
                source: enum_column(row, 4)?,
                revision: row.get(3)?,
            },
        ))
    })? {
        targets.entry(entry).or_default().push(target);
    }
    Ok(targets)
}

/// The tool definition row `id` holds, decoded once per process by its `digest`: a
/// content digest, so every session's row of the same digest holds the same definition.
fn definition(db: &Db, id: i64, digest: Vec<u8>) -> DbResult<std::sync::Arc<ToolDefinition>> {
    use std::sync::{Arc, LazyLock, Mutex, PoisonError};
    type Decoded = HashMap<Vec<u8>, Arc<ToolDefinition>>;
    static DECODED: LazyLock<Mutex<Decoded>> = LazyLock::new(Mutex::default);
    let decoded = || DECODED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(tool) = decoded().get(&digest) {
        return Ok(tool.clone());
    }
    let tool = db.query_row(
        "SELECT name, description, input_schema FROM tool_definition WHERE id = ?1",
        params![id],
        |row| {
            Ok(ToolDefinition {
                name: row.get(0)?,
                description: row.get(1)?,
                input_schema: parse_json(&row.get::<String>(2)?)?,
            })
        },
    )?;
    let tool = tool.ok_or_else(|| corrupt("tool definition is missing"))?;
    Ok(decoded()
        .entry(digest)
        .or_insert_with(|| Arc::new(tool))
        .clone())
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
        |row| parsed(row.get(1)?),
    )?;
    // Mode editions by the entry that recorded them.
    let mode_capabilities = grouped(
        db,
        events.scoped(
            "SELECT m.entry, c.capability FROM mode_capability c \
             JOIN mode m ON m.name = c.mode_name AND m.version = c.mode_version {scope}",
            "m.entry",
        )?,
        |row| parsed(row.get(1)?),
    )?;
    let editions = events.scoped(
        "SELECT entry, instructions, hint FROM mode {scope}",
        "entry",
    )?;
    let mut editions = keyed(db, editions, |row| {
        let capabilities = mode_capabilities.get(&row.get::<i64>(0)?);
        Ok(Edition {
            capabilities: (capabilities.into_iter().flatten().cloned())
                .collect::<CapabilitySet>()
                .try_into()
                .map_err(|error: InteractiveListed| corrupt(error.to_string()))?,
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
                capabilities: take(&mut capabilities, entry).into_iter().collect(),
                location: location(row.get(3)?, row.get(4)?)?,
            })
        },
    )?;
    // Every other entry that applied a mode changed it.
    for (entry, name) in modes {
        let definition = editions.remove(&entry);
        let mode = ModeSelection { name, definition };
        let capabilities = take(&mut capabilities, entry).into_iter().collect();
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
            "SELECT t.context, t.tool, d.digest FROM model_context_tool t \
             JOIN tool_definition d ON d.id = t.tool \
             WHERE t.context IN (SELECT c.entry FROM model_context c {scope}) \
             ORDER BY t.context, t.position",
            "c.entry",
        )?,
        |row| definition(db, row.get(1)?, row.get(2)?),
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

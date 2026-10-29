use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::{
    db::{enum_column, params},
    target::TargetName,
};

use super::*;

/// A locator, never a credential value. Secret manager references name externally
/// managed entries; the store neither resolves them nor executes instructions.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(
    try_from = "CredentialReferenceInput",
    into = "CredentialReferenceInput"
)]
pub struct CredentialReference {
    kind: CredentialReferenceKind,
    name: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CredentialReferenceInput {
    pub kind: CredentialReferenceKind,
    pub name: String,
}
impl TryFrom<CredentialReferenceInput> for CredentialReference {
    type Error = Error;
    fn try_from(input: CredentialReferenceInput) -> Result<Self> {
        let valid = match input.kind {
            CredentialReferenceKind::EnvironmentVariable => {
                input
                    .name
                    .bytes()
                    .next()
                    .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                    && input
                        .name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            }
            CredentialReferenceKind::SecretManager => input
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_./:-".contains(&b)),
        };
        if input.name.is_empty() || input.name.len() > 1024 || !valid {
            return Err(Error::CredentialReference);
        }
        Ok(Self {
            kind: input.kind,
            name: input.name,
        })
    }
}
impl From<CredentialReference> for CredentialReferenceInput {
    fn from(value: CredentialReference) -> Self {
        Self {
            kind: value.kind,
            name: value.name,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentInput {
    pub name: Text,
    /// Startup/setup instructions must not contain plaintext credentials.
    pub instructions: Text,
    /// A configured target name, not a connection string.
    pub target: Option<TargetName>,
    pub endpoints: Vec<Endpoint>,
    pub credentials: Vec<CredentialReference>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Environment {
    pub id: EnvironmentId,
    pub details: EnvironmentInput,
    pub actor: AgentId,
    pub at: DateTime<Utc>,
}

impl Store {
    /// Appends environment metadata only. This does not provision, connect, read
    /// files or resolve credentials. Prose is not a secret-scanning boundary.
    pub fn record_environment(
        &self,
        actor: &AgentId,
        input: &EnvironmentInput,
    ) -> Result<EnvironmentId> {
        self.write(|db| {
            let owner = actor_row(db, actor)?;
            let id = EnvironmentId(db.insert(
                "INSERT INTO environment(name,instructions,target,actor,at) VALUES (?1,?2,?3,?4,?5)",
                params![&input.name, &input.instructions, input.target.as_ref().map(TargetName::as_str), owner, now()],
            )?);
            for (position, endpoint) in input.endpoints.iter().enumerate() {
                db.execute(
                    "INSERT INTO environment_endpoint(environment,position,address) VALUES (?1,?2,?3)",
                    params![id.0, position, endpoint.as_str()],
                )?;
            }
            for (position, credential) in input.credentials.iter().enumerate() {
                db.execute(
                    "INSERT INTO environment_credential(environment,position,kind,reference) VALUES (?1,?2,?3,?4)",
                    params![id.0, position, credential.kind, &credential.name],
                )?;
            }
            let detail = Detail::Record(EventRecord::Environment(id));
            audit(db, actor, EventKind::EnvironmentRecorded, detail, &[])?;
            Ok(id)
        })
    }

    pub fn environments(&self, page: Page) -> Result<Listing<Environment>> {
        self.read(|db| {
            let mut environments = rows(
                db,
                concat!("SELECT e.id,e.name,e.instructions,e.target,e.at,", actor_columns!(), " FROM environment e JOIN actor a ON a.id=e.actor WHERE e.id>?1 ORDER BY e.id LIMIT ?2"),
                params![page.after, page.limit()],
                |row| {
                    Ok(Environment {
                        id: EnvironmentId(row.get(0)?),
                        details: EnvironmentInput {
                            name: row.get::<String>(1)?.parse()?,
                            instructions: row.get::<String>(2)?.parse()?,
                            target: row
                                .get::<Option<String>>(3)?
                                .map(|name| name.parse().map_err(|_| corrupt("invalid stored target name")))
                                .transpose()?,
                            endpoints: Vec::new(),
                            credentials: Vec::new(),
                        },
                        at: time(row.get(4)?)?,
                        actor: actor_column(row, 5)?,
                    })
                },
            )?;
            for environment in &mut environments {
                environment.details.endpoints = rows(
                    db,
                    "SELECT address FROM environment_endpoint WHERE environment=?1 ORDER BY position",
                    params![environment.id.0],
                    |row| Ok(row.get::<String>(0)?.parse()?),
                )?;
                environment.details.credentials = rows(
                    db,
                    "SELECT kind,reference FROM environment_credential WHERE environment=?1 ORDER BY position",
                    params![environment.id.0],
                    |row| {
                        Ok(CredentialReference {
                            kind: enum_column(row, 0)?,
                            name: row.get(1)?,
                        })
                    },
                )?;
            }
            Ok(Listing::keyset(environments, page))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::tests::{fixture, text},
        *,
    };

    #[test]
    fn environments_hold_references_not_credentials() {
        let (_dir, store, actor) = fixture();
        let reference = serde_json::json!({"kind":"environment_variable","name":"TEST_TOKEN","value":"not-a-real-secret"});
        assert!(serde_json::from_value::<CredentialReference>(reference).is_err());
        assert!("https://user:password@host/".parse::<Endpoint>().is_err());
        assert!("https://host/?token=secret".parse::<Endpoint>().is_err());
        let input = EnvironmentInput {
            name: text("container"),
            instructions: text("Inject TEST_TOKEN from the external secret manager"),
            target: Some("sandbox".parse().unwrap()),
            endpoints: vec!["http://localhost:8080/".parse().unwrap()],
            credentials: vec![
                CredentialReferenceInput {
                    kind: CredentialReferenceKind::EnvironmentVariable,
                    name: "TEST_TOKEN".into(),
                }
                .try_into()
                .unwrap(),
            ],
        };
        store.record_environment(&actor, &input).unwrap();
        let environments = store.environments(Page::default()).unwrap();
        assert_eq!(
            environments[0].details.target.as_ref().unwrap().as_str(),
            "sandbox"
        );
    }
}

//! A target's SSH settings.
use std::collections::BTreeMap;
use std::num::NonZeroU16;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook_base::enums::Kinds;
use skyhook_worker::{
    op::TargetPath,
    origin::ssh::{SshOptionName, SshUser, SshValue},
    secret::Credential,
};

use super::Password;

/// A target's SSH settings. How it authenticates follows from which of `key` and
/// `password` are set.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(from = "RawSshOptions", into = "RawSshOptions")]
pub struct SshOptions {
    pub user: Option<SshUser>,
    pub port: Option<NonZeroU16>,
    pub auth: SshAuth,
    /// Forward the agent the connection authenticated with.
    pub forward_agent: bool,
    pub options: BTreeMap<SshOptionName, SshValue>,
}

/// How an SSH target authenticates. OpenSSH's default key files are never offered.
#[derive(Clone, Debug, Default, PartialEq, Eq, Kinds)]
#[kinds(name = SshAuthKind, serde(rename_all = "snake_case"))]
pub enum SshAuth {
    /// Only keys already in the origin's agent.
    #[default]
    Agent,
    /// One private key on the SSH origin, with the passphrase of an encrypted one.
    Key {
        path: TargetPath,
        password: Option<Password>,
    },
    /// A password, offering no keys.
    Password { password: Password },
}

// `SshOptions` as written.
#[serde_with::skip_serializing_none]
#[derive(Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields, rename = "SshOptions")]
#[schemars(rename = "SshOptions")]
struct RawSshOptions {
    /// Remote user; defaults to the username on the SSH origin
    user: Option<SshUser>,
    /// SSH port; defaults to 22
    port: Option<NonZeroU16>,
    /// Private-key path on the SSH origin, the only key offered. Without key or password, only keys in the origin's SSH agent are offered
    key: Option<TargetPath>,
    /// With key, its passphrase; without, password authentication offering no keys. Read on the SSH origin at each connection
    // A default keeps the field optional, which `schema_with` alone would require.
    #[serde(default)]
    #[schemars(schema_with = "optional_password")]
    password: Option<Password>,
    /// Forward the SSH agent the connection authenticated with, so remote commands receive SSH_AUTH_SOCK
    #[serde(default)]
    #[schemars(transform = undocumented_default)]
    forward_agent: bool,
    /// Extra ssh_config options written verbatim, e.g. {"ProxyCommand": "sudo -n -u deploy ssh -W %h:%p gateway.internal"}. User, Port and IdentityFile belong in user, port and key; ProxyJump in the target's via
    #[serde(default)]
    #[schemars(transform = undocumented_default)]
    options: BTreeMap<SshOptionName, SshValue>,
}

/// The schema leaves a default implicit, as for any other omitted setting.
fn undocumented_default(schema: &mut schemars::Schema) {
    schema.remove("default");
}

/// An optional credential's schema: its forms and `null` in one flat `anyOf`, where
/// `Option` would nest them.
fn optional_password(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut schema = <Credential as JsonSchema>::json_schema(generator);
    if let Some(forms) = (schema.get_mut("anyOf")).and_then(serde_json::Value::as_array_mut) {
        forms.push(serde_json::json!({"type": "null"}));
    }
    schema
}

impl From<RawSshOptions> for SshOptions {
    fn from(raw: RawSshOptions) -> Self {
        let auth = match (raw.key, raw.password) {
            (Some(path), password) => SshAuth::Key { path, password },
            (None, Some(password)) => SshAuth::Password { password },
            (None, None) => SshAuth::Agent,
        };
        Self {
            user: raw.user,
            port: raw.port,
            auth,
            forward_agent: raw.forward_agent,
            options: raw.options,
        }
    }
}

impl From<SshOptions> for RawSshOptions {
    fn from(options: SshOptions) -> Self {
        let (key, password) = match options.auth {
            SshAuth::Agent => (None, None),
            SshAuth::Key { path, password } => (Some(path), password),
            SshAuth::Password { password } => (None, Some(password)),
        };
        Self {
            user: options.user,
            port: options.port,
            key,
            password,
            forward_agent: options.forward_agent,
            options: options.options,
        }
    }
}

/// Schemas describe the fields as written.
impl JsonSchema for SshOptions {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        RawSshOptions::schema_name()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        RawSshOptions::json_schema(generator)
    }
}

impl SshAuth {
    /// The password this method types at a prompt.
    pub fn password(&self) -> Option<&Password> {
        match self {
            Self::Agent => None,
            Self::Key { password, .. } => password.as_ref(),
            Self::Password { password } => Some(password),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authentication_follows_from_the_fields_set() {
        let auth = |ssh: &str| crate::yaml::parse::<SshOptions>(ssh).map(|ssh| ssh.auth);
        let password = || Password::Given(Credential::Env("PW".into()));
        for (ssh, expected) in [
            ("{}", SshAuth::Agent),
            (
                "{password: {env: PW}}",
                SshAuth::Password {
                    password: password(),
                },
            ),
            (
                "{key: ~/k, password: {env: PW}}",
                SshAuth::Key {
                    path: "~/k".into(),
                    password: Some(password()),
                },
            ),
        ] {
            assert_eq!(auth(ssh).unwrap(), expected, "{ssh}");
        }
    }
}

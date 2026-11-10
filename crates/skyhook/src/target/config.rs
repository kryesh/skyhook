//! The configured targets.
use std::{collections::BTreeMap, sync::Arc};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook_base::enums::Kinds;
use skyhook_worker::{op::TargetPath, secret::Credential, target::Os};

use super::{SshAuth, SshOptions, TargetDefinition, TargetRegistry, WinrmOptions};
use crate::target::TargetError;
use skyhook_tools::target::TargetName;

/// Named targets. A later configuration layer replaces a same-named target whole.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub struct TargetsConfig {
    pub entries: BTreeMap<String, TargetConfig>,
}

impl TargetsConfig {
    /// Validate only the supplied definitions.
    /// Missing route references may be supplied by later config layers or at runtime.
    pub(crate) fn validate_structure(&self) -> Result<(), TargetError> {
        let definitions = (self.definitions()?.into_iter())
            .map(|definition| (definition.name.clone(), definition))
            .collect();
        super::registry::validate_route_cycles(&definitions)?;
        super::registry::validate_passwords(&definitions)
    }

    pub(crate) fn definitions(&self) -> Result<Vec<TargetDefinition>, TargetError> {
        self.entries
            .iter()
            .map(|(name, config)| TargetDefinition::from_config(name.clone(), config.clone()))
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    pub r#type: ConnectionKind,
    /// Hostname or IP address
    pub host: String,
    /// The destination's operating system; defaults to linux for ssh and windows for winrm
    pub os: Option<Os>,
    #[serde(default)]
    #[schemars(extend("default" = {}))]
    pub ssh: SshOptions,
    pub winrm: Option<WinrmOptions>,
    /// Default directory on the destination; defaults to its login directory
    #[serde(default = "default_workspace")]
    pub workspace: TargetPath,
    /// Jump through this target (ProxyJump) within the SSH connection origin starts; the jump must share that origin
    pub via: Option<String>,
    /// Target whose Skyhook shim starts the SSH or WinRM connection, using its key paths and agents; defaults to root
    pub origin: Option<String>,
}

/// Written with the effective settings of its own transport alone, defaults included.
impl Serialize for TargetConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[serde_with::skip_serializing_none]
        #[derive(Serialize)]
        struct Written<'a> {
            r#type: ConnectionKind,
            host: &'a str,
            os: Option<Os>,
            ssh: Option<&'a SshOptions>,
            winrm: Option<&'a WinrmOptions>,
            workspace: &'a TargetPath,
            via: Option<&'a str>,
            origin: Option<&'a str>,
        }
        Written {
            r#type: self.r#type,
            host: &self.host,
            os: self.os,
            ssh: (self.r#type == ConnectionKind::Ssh).then_some(&self.ssh),
            winrm: self.winrm.as_ref(),
            workspace: &self.workspace,
            via: self.via.as_deref(),
            origin: self.origin.as_deref(),
        }
        .serialize(serializer)
    }
}

/// How a target is reached: its transport's settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Kinds)]
#[serde(rename_all = "snake_case")]
#[kinds(
    name = ConnectionKind,
    derive(JsonSchema),
    schemars(rename = "Transport"),
    doc = "The transport of a named target; the session host is not a named target"
)]
pub enum Connection {
    Ssh(SshOptions),
    Winrm(WinrmOptions),
}

impl ConnectionKind {
    /// The operating system a target of this transport runs unless it declares one.
    pub const fn default_os(self) -> Os {
        match self {
            Self::Ssh => Os::Linux,
            Self::Winrm => Os::Windows,
        }
    }
}

impl Connection {
    pub fn ssh(&self) -> Option<&SshOptions> {
        match self {
            Self::Ssh(ssh) => Some(ssh),
            Self::Winrm(_) => None,
        }
    }

    /// The user this connection logs in as, when its settings name one.
    pub fn user(&self) -> Option<String> {
        match self {
            Self::Ssh(ssh) => ssh.user.clone().map(String::from),
            Self::Winrm(winrm) => Some(winrm.auth.user()),
        }
    }

    /// The password this connection reads.
    pub fn password(&self) -> Option<&Password> {
        match self {
            Self::Ssh(ssh) => ssh.auth.password(),
            Self::Winrm(winrm) => Some(winrm.auth.password()),
        }
    }

    /// This connection as a session holds a configured target's: its password is
    /// the configuration's, named and not copied.
    fn configured(self) -> Self {
        let configured = Password::Configured;
        match self {
            Self::Ssh(mut ssh) => {
                ssh.auth = match ssh.auth {
                    SshAuth::Agent => SshAuth::Agent,
                    SshAuth::Key { path, password } => SshAuth::Key {
                        path,
                        password: password.map(|_| configured),
                    },
                    SshAuth::Password { .. } => SshAuth::Password {
                        password: configured,
                    },
                };
                Self::Ssh(ssh)
            }
            Self::Winrm(mut winrm) => {
                winrm.auth = winrm.auth.with_password(configured);
                Self::Winrm(winrm)
            }
        }
    }
}

/// A target's password: the credential its definition gives or, for a target the
/// configuration defines, the one the configuration gives it. A session names that
/// one and never copies it, so its journal holds no credential the configuration
/// does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Password {
    Configured,
    #[serde(untagged)]
    Given(Credential),
}

/// Every written password is a credential.
impl<'de> Deserialize<'de> for Password {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Credential::deserialize(deserializer).map(Self::Given)
    }
}

impl Password {
    /// The credential the definition itself gives.
    pub fn given(&self) -> Option<&Credential> {
        match self {
            Self::Given(credential) => Some(credential),
            Self::Configured => None,
        }
    }
}

/// The configuration's targets, split once: each as a session holds it, beside the
/// password the configuration gives it.
pub(crate) struct ConfiguredTargets(Vec<(TargetDefinition, Option<Credential>)>);

impl ConfiguredTargets {
    pub(crate) fn of(configured: Vec<TargetDefinition>) -> Self {
        let split = |mut target: TargetDefinition| {
            let password = target.connection.password().and_then(Password::given);
            let password = password.cloned();
            target.connection = target.connection.configured();
            (target, password)
        };
        Self(configured.into_iter().map(split).collect())
    }

    /// The targets a new session starts with.
    pub(crate) fn definitions(&self) -> Vec<TargetDefinition> {
        (self.0.iter().map(|(target, _)| target.clone())).collect()
    }

    /// The passwords a session over `session`'s targets reads: those of the targets
    /// it holds as the configuration now defines them, or has yet to start with. A
    /// target it started with otherwise names another host or route, which the
    /// configuration's password is not for.
    pub(crate) fn passwords(&self, session: &TargetRegistry) -> ConfiguredPasswords {
        let read = |(target, password): &(TargetDefinition, Option<Credential>)| {
            let held = session.get(&target.name).ok();
            let pinned = held.is_none_or(|held| held == *target);
            Some((target.name.clone(), password.clone().filter(|_| pinned)?))
        };
        ConfiguredPasswords(Arc::new(self.0.iter().filter_map(read).collect()))
    }
}

/// The passwords a session reads from the configuration, by target.
#[derive(Clone, Default)]
pub(crate) struct ConfiguredPasswords(Arc<BTreeMap<TargetName, Credential>>);

/// A configured password the configuration no longer gives its target as the
/// session holds it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the configuration no longer gives a target on the route its password")]
pub(crate) struct PasswordUnconfigured;

impl ConfiguredPasswords {
    /// The credential `target`'s `password` is.
    pub(crate) fn credential<'a>(
        &'a self,
        target: &TargetName,
        password: &'a Password,
    ) -> Result<&'a Credential, PasswordUnconfigured> {
        match password {
            Password::Given(credential) => Ok(credential),
            Password::Configured => self.0.get(target).ok_or(PasswordUnconfigured),
        }
    }
}

fn default_workspace() -> TargetPath {
    ".".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_implicit() {
        let valid = |text: &str| {
            crate::yaml::parse::<TargetsConfig>(text)
                .is_ok_and(|config| config.validate_structure().is_ok())
        };
        for text in [
            "root:\n  type: ssh\n  host: root",
            "a:\n  type: ssh\n  host: a\n  via: root",
        ] {
            assert!(!valid(text), "{text}");
        }
        assert!(valid("a:\n  type: ssh\n  host: a"));
        // A later layer or the runtime may supply a missing name.
        assert!(valid("a:\n  type: ssh\n  host: a\n  via: b"));
    }
}

//! The configured targets.
use std::{collections::BTreeMap, sync::Arc};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook_base::enums::{Kinds, NamedEnum};
use skyhook_worker::{op::TargetPath, secret::Credential, target::Os};

use super::{SshAuth, SshOptions, TargetDefinition, TargetRegistry, WinrmOptions};
use crate::config::Refused;
use crate::target::{TargetEdge, TargetError};
use skyhook_base::decode::{Decode, Expected, Segment};
use skyhook_tools::target::{TargetName, TargetRef};

/// Named targets. A later configuration layer replaces a same-named target whole.
#[derive(Clone, Debug, Default, Serialize, Decode)]
#[serde(transparent)]
pub struct TargetsConfig {
    pub entries: BTreeMap<String, TargetConfig>,
}

impl TargetsConfig {
    /// Every refusal of the configured targets, by the path of the setting refused: each
    /// target's own settings, then its route through the others. What a route refuses of
    /// another target is refused once, at that target. Only a `complete` configuration
    /// refuses a route through a target it does not define.
    pub(crate) fn refusals(&self, complete: bool) -> Vec<Refused> {
        let mut refusals = Vec::new();
        let mut definitions = BTreeMap::new();
        for (name, config) in &self.entries {
            match TargetDefinition::from_config(name.clone(), config.clone()) {
                Ok(definition) => {
                    definitions.insert(definition.name.clone(), definition);
                }
                Err(error) => refusals.push(refusal(name, &error, &definitions)),
            }
        }
        for name in definitions.keys() {
            match super::registry::validate_entry(&definitions, name) {
                Err(TargetError::UnknownReference { .. }) if !complete => {}
                Err(error) if error.target().is_some_and(|target| target != name) => {}
                Err(error) => refusals.push(refusal(name.as_str(), &error, &definitions)),
                Ok(()) => {}
            }
        }
        refusals
    }

    pub(crate) fn definitions(&self) -> Result<Vec<TargetDefinition>, TargetError> {
        self.entries
            .iter()
            .map(|(name, config)| TargetDefinition::from_config(name.clone(), config.clone()))
            .collect()
    }
}

#[serde_with::skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, Decode)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    /// Hostname or IP address
    pub host: String,
    /// The destination's operating system; defaults to linux for ssh and windows for winrm
    pub os: Option<Os>,
    pub transport: Connection,
    /// Default directory on the destination; defaults to its login directory
    #[serde(default = "default_workspace")]
    pub workspace: TargetPath,
    /// Jump through this target (ProxyJump) within the SSH connection origin starts; the jump must share that origin
    pub via: Option<String>,
    /// Target whose Skyhook shim starts the SSH or WinRM connection, using its key paths and agents; defaults to root
    pub origin: Option<String>,
}

/// How the target is reached: one transport and its settings
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema, Kinds)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[schemars(rename = "Transport")]
#[kinds(
    name = ConnectionKind,
    derive(JsonSchema),
    schemars(rename = "TransportKind"),
    doc = "The transport of a named target; the session host is not a named target"
)]
pub enum Connection {
    Ssh(SshOptions),
    Winrm(WinrmOptions),
}

impl Decode for Connection {
    fn decode(
        node: &skyhook_base::decode::Node,
        cx: &mut skyhook_base::decode::Context,
    ) -> Option<Self> {
        let names = ConnectionKind::ALL.map(ConnectionKind::as_str);
        skyhook_base::decode::variant(node, cx, &names, |key, value, cx| {
            match ConnectionKind::parse(key)? {
                ConnectionKind::Ssh => SshOptions::decode(value, cx).map(Self::Ssh),
                ConnectionKind::Winrm => WinrmOptions::decode(value, cx).map(Self::Winrm),
            }
        })
    }
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
impl Decode for Password {
    fn decode(
        node: &skyhook_base::decode::Node,
        cx: &mut skyhook_base::decode::Context,
    ) -> Option<Self> {
        Credential::decode(node, cx).map(Self::Given)
    }
}

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

/// `error` of the target `name`, at the setting it refuses; an unknown reference lists
/// the names the reference could take instead.
fn refusal(
    name: &str,
    error: &TargetError,
    definitions: &BTreeMap<TargetName, TargetDefinition>,
) -> Refused {
    let mut path = vec![Segment::Key("targets".into()), Segment::Key(name.into())];
    let field = error.field().into_iter();
    path.extend(field.map(|key| Segment::Key(key.into())));
    match error {
        TargetError::UnknownReference {
            edge, reference, ..
        } => {
            let root = (*edge == TargetEdge::Origin).then_some(TargetRef::Root);
            let others = (definitions.keys()).filter(|other| other.as_str() != name);
            let valid = root
                .into_iter()
                .chain(others.cloned().map(TargetRef::Named));
            let message = format!("{error}, {}", Expected(valid.collect()));
            let about = vec![
                Segment::Key("targets".into()),
                Segment::Key(reference.to_string()),
            ];
            Refused::new(path, message).about(about)
        }
        error => Refused::new(path, error),
    }
}

fn default_workspace() -> TargetPath {
    ".".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusals(text: &str) -> Vec<String> {
        let targets = crate::yaml::parse::<TargetsConfig>(text).unwrap();
        let refusals = targets.refusals(true).into_iter();
        refusals
            .map(|refused| format!("{}: {}", refused.path, refused.message))
            .collect()
    }

    #[test]
    fn root_is_implicit() {
        for text in [
            "root:\n  host: root\n  transport: {ssh: {}}",
            "a:\n  host: a\n  transport: {ssh: {}}\n  via: root",
        ] {
            assert_eq!(refusals(text).len(), 1, "{text}");
        }
        assert!(refusals("a:\n  host: a\n  transport: {ssh: {}}\n  origin: root").is_empty());
    }

    /// A refusal names the target's setting it refuses, and a reference refused lists
    /// what it could name instead.
    #[test]
    fn refusals_name_the_setting_and_references_name_configured_targets() {
        let two = "a:\n  host: a\n  transport: {ssh: {}}\nb:\n  host: b\n  transport: {ssh: {}}\n";
        assert_eq!(
            refusals(&format!("{two}  via: c")),
            ["targets.b.via: target `b` references unknown via target `c`, expected one of `a`"]
        );
        assert_eq!(
            refusals(&format!("{two}  origin: c")),
            [
                "targets.b.origin: target `b` references unknown origin target `c`, expected one \
              of `root`, `a`"
            ]
        );
        for valid in ["via: a", "origin: a", "origin: root"] {
            assert!(refusals(&format!("{two}  {valid}")).is_empty(), "{valid}");
        }
        // A route through another target's bad setting is that target's refusal alone.
        let gateway = "app:\n  host: app\n  transport: {ssh: {}}\n  origin: gateway\ngateway:\n  host: g\n  transport: {ssh: {}}\n";
        assert_eq!(
            refusals(&format!("{gateway}  via: missing")),
            [
                "targets.gateway.via: target `gateway` references unknown via target `missing`, \
              expected one of `app`"
            ]
        );
        let missing = crate::yaml::parse::<TargetsConfig>("w:\n  os: windows").unwrap_err();
        assert_eq!(
            missing.to_string(),
            "2:3: `w`: missing required fields `host`, `transport`"
        );
    }
}

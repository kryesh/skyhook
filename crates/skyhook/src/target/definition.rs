//! Target definitions and the routes connections follow.

use serde::Serialize;

use super::options::SshOption;
use super::{Connection, ConnectionKind, SshOptions, TargetConfig};
use crate::target::{TargetEdge, TargetError};
use crate::tool::policy::{Capability, PermissionUse, ResourceId};
use skyhook_base::enums::NamedEnum;
use skyhook_tools::target::{TargetName, TargetRef};
use skyhook_worker::{
    origin::ssh::{HostKeyChecking, HostKeyUpdates},
    target::Os,
};

/// Where a target's definition comes from.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum TargetSource {
    Config,
    Session,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct TargetDefinition {
    pub name: TargetName,
    pub host: String,
    /// The operating system the target declares, which picks how its shim is deployed.
    pub os: Os,
    pub connection: Connection,
    pub workspace: skyhook_worker::op::TargetPath,
    /// A native jump (ProxyJump) within the SSH connection `origin` starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<TargetName>,
    /// The target whose shim starts the SSH connection; `None` is root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<TargetName>,
    pub source: TargetSource,
    pub revision: u64,
}

impl TargetDefinition {
    /// A configured definition; the registry stamps session-added ones when it
    /// installs them.
    pub fn from_config(name: String, config: TargetConfig) -> Result<Self, TargetError> {
        let link = |edge, link: Option<String>| {
            link.map(|link| {
                TargetRef::try_from(link).map_err(|_| TargetError::InvalidReference { edge })
            })
            .transpose()
        };
        // `origin: root` spells the default; a jump through root is no route.
        let origin =
            link(TargetEdge::Origin, config.origin)?.and_then(|origin| origin.name().cloned());
        let via = match link(TargetEdge::Via, config.via)? {
            Some(TargetRef::Root) => return Err(TargetError::RootCannotBeJump),
            via => via.and_then(|via| via.name().cloned()),
        };
        let foreign = |settings| TargetError::ForeignSettings {
            transport: config.r#type,
            settings,
        };
        let connection = match (config.r#type, config.winrm) {
            (ConnectionKind::Ssh, None) => Connection::Ssh(config.ssh),
            (ConnectionKind::Ssh, Some(_)) => return Err(foreign(ConnectionKind::Winrm)),
            (ConnectionKind::Winrm, _) if config.ssh != Default::default() => {
                return Err(foreign(ConnectionKind::Ssh));
            }
            (ConnectionKind::Winrm, Some(winrm)) => Connection::Winrm(winrm),
            (ConnectionKind::Winrm, None) => {
                return Err(TargetError::MissingSettings(ConnectionKind::Winrm));
            }
        };
        let definition = Self {
            name: TargetName::try_from(name)?,
            host: config.host,
            os: config.os.unwrap_or(config.r#type.default_os()),
            connection,
            workspace: config.workspace,
            via,
            origin,
            source: TargetSource::Config,
            revision: 1,
        };
        definition.validate()?;
        Ok(definition)
    }

    pub fn validate(&self) -> Result<(), TargetError> {
        validate_host(&self.host)?;
        if self.via.is_some() && self.via == self.origin {
            return Err(TargetError::ViaIsOrigin(self.name.clone()));
        }
        let ssh = match &self.connection {
            Connection::Ssh(ssh) => ssh,
            Connection::Winrm(_) if self.via.is_some() => {
                return Err(TargetError::WinrmVia(self.name.clone()));
            }
            Connection::Winrm(_) => return Ok(()),
        };
        // Option values are never retained in errors: they may be sensitive.
        for (key, value) in &ssh.options {
            match SshOption::parse(key) {
                SshOption::Dedicated(field) => {
                    return Err(TargetError::DedicatedSshOption(key.to_string(), field));
                }
                SshOption::Reserved => return Err(TargetError::ReservedSshOption(key.to_string())),
                SshOption::ProxyCommand if self.via.is_some() => {
                    return Err(TargetError::ProxyCommandWithVia(self.name.clone()));
                }
                SshOption::StrictHostKeyChecking
                    if HostKeyChecking::of(value.as_str()).is_none() =>
                {
                    return Err(TargetError::InvalidHostKeyChecking(key.to_string()));
                }
                SshOption::UpdateHostKeys if HostKeyUpdates::of(value.as_str()).is_none() => {
                    return Err(TargetError::InvalidHostKeyUpdates(key.to_string()));
                }
                SshOption::ProxyCommand
                | SshOption::StrictHostKeyChecking
                | SshOption::UpdateHostKeys
                | SshOption::ConnectTimeout
                | SshOption::Free => {}
            }
        }
        Ok(())
    }
}

/// How a connection reaches its destination: a snapshot of the registry, which may
/// since have changed.
#[derive(Clone, Debug, PartialEq)]
pub struct Route {
    /// The route to the target whose shim starts this connection; `None` is root.
    pub origin: Option<Box<Route>>,
    /// The `via` targets the connection's SSH process jumps through, outermost first.
    pub jumps: Vec<SshJump>,
    pub destination: TargetDefinition,
}

/// A `via` target: an SSH hop within the connection its destination's origin starts.
#[derive(Clone, Debug, PartialEq)]
pub struct SshJump {
    pub name: TargetName,
    pub revision: u64,
    pub host: String,
    pub ssh: SshOptions,
}

impl Route {
    /// Each target on the route with its revision, outermost first.
    pub fn identity(&self) -> Revisions {
        let mut identity = (self.origin.as_ref()).map_or_else(Vec::new, |origin| origin.identity());
        identity.extend((self.jumps.iter()).map(|jump| (jump.name.clone(), jump.revision)));
        identity.push((self.destination.name.clone(), self.destination.revision));
        identity
    }

    /// Connecting needs target approval.
    pub fn permissions(&self) -> Vec<PermissionUse> {
        let resource = ResourceId::route(self.destination.name.clone(), self.identity());
        vec![PermissionUse::exact(Capability::Targets, resource)]
    }

    pub fn authorization_arguments(&self) -> RouteArguments {
        let destination = &self.destination;
        RouteArguments {
            destination: destination.name.clone(),
            origin: destination.origin.clone(),
            route: self.hops(),
        }
    }

    /// Each target on the route as its definition reaches it, outermost first.
    fn hops(&self) -> Vec<RouteHop> {
        let mut hops = (self.origin.as_ref()).map_or_else(Vec::new, |origin| origin.hops());
        hops.extend(self.jumps.iter().map(|jump| RouteHop {
            name: jump.name.clone(),
            host: jump.host.clone(),
            user: jump.ssh.user.clone().map(String::from),
        }));
        let destination = &self.destination;
        hops.push(RouteHop {
            name: destination.name.clone(),
            host: destination.host.clone(),
            user: destination.connection.user(),
        });
        hops
    }
}

/// What an approval of a connection along a route is shown of it.
#[derive(Clone, Debug, serde::Serialize)]
pub struct RouteArguments {
    pub destination: TargetName,
    pub origin: Option<TargetName>,
    /// Every target the connection passes through, outermost first.
    pub route: Vec<RouteHop>,
}

/// A target on a route as its definition reaches it: an approval shows where each
/// one leads, so a redefined origin or jump is seen for what it now is.
#[serde_with::skip_serializing_none]
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct RouteHop {
    pub name: TargetName,
    pub host: String,
    /// Who it logs in as; none is the user OpenSSH picks where its connection starts.
    pub user: Option<String>,
}

/// The revision of each target something depends on, outermost first.
pub type Revisions = Vec<(TargetName, u64)>;

/// A host is nonempty, without whitespace or control characters.
fn validate_host(host: &str) -> Result<(), TargetError> {
    let invalid = |character: char| character.is_control() || character.is_whitespace();
    match host.is_empty() || host.chars().any(invalid) {
        true => Err(TargetError::InvalidHost(host.to_owned())),
        false => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::TargetField;
    use skyhook_tools::error::AdmissionError;
    use std::assert_matches;

    impl TargetDefinition {
        /// This SSH target's settings.
        pub(crate) fn ssh_mut(&mut self) -> &mut SshOptions {
            match &mut self.connection {
                Connection::Ssh(ssh) => ssh,
                Connection::Winrm(_) => panic!("{} is not an SSH target", self.name),
            }
        }
    }

    fn target(name: &str, via: Option<&str>) -> TargetDefinition {
        crate::target::tests::definition(name, ".", via)
    }

    fn name(name: &str) -> TargetName {
        name.parse().unwrap()
    }

    #[test]
    fn ssh_options_leave_routing_and_managed_settings_to_their_fields() {
        let option = |key: &str, value: &str, via: Option<&str>| {
            let mut definition = target("x", via);
            let options = &mut definition.ssh_mut().options;
            options.insert(key.parse().unwrap(), value.parse().unwrap());
            definition.validate()
        };
        assert_matches!(
            option("ProxyCommand", "nc %h %p", Some("jump")),
            Err(TargetError::ProxyCommandWithVia(_))
        );
        assert_matches!(
            option("SendEnv", "*", None),
            Err(TargetError::ReservedSshOption(_))
        );
        assert_matches!(
            option("proxyjump", "a", None),
            Err(TargetError::DedicatedSshOption(_, TargetField::Via))
        );
        assert_matches!(
            option("stricthostkeychecking", "always", None),
            Err(TargetError::InvalidHostKeyChecking(_))
        );
        assert_matches!(
            option("UpdateHostKeys", "accept-new", None),
            Err(TargetError::InvalidHostKeyUpdates(_))
        );
        // Managed options with a field of their own name it.
        let error = option("User", "debian", None).unwrap_err().to_string();
        assert!(error.contains("`ssh.user"), "{error}");
    }

    #[test]
    fn admission_errors_never_carry_submitted_values_or_aliases() {
        let private = || "private-submitted-target".to_owned();
        let named = || name(&private());
        for error in [
            TargetError::InvalidHost(private()),
            TargetError::ReservedSshOption(private()),
            TargetError::InvalidHostKeyChecking(private()),
            TargetError::InvalidHostKeyUpdates(private()),
            TargetError::DedicatedSshOption(private(), TargetField::SshKey),
            TargetError::ProxyCommandWithVia(named()),
            TargetError::ViaIsOrigin(named()),
            TargetError::OriginMismatch {
                target: named(),
                jump: named(),
                origin: named().into(),
                jump_origin: named().into(),
            },
            TargetError::Unknown(named()),
            TargetError::UnknownReference {
                target: named(),
                edge: TargetEdge::Via,
                reference: named(),
            },
            TargetError::Cycle(named()),
            TargetError::SecretsInOneConnection {
                target: named(),
                first: named(),
                second: named(),
            },
            TargetError::WinrmVia(named()),
            TargetError::WinrmJump {
                target: named(),
                jump: named(),
            },
        ] {
            let diagnostic = AdmissionError::from(error).diagnostic();
            let saved = serde_json::to_string(&diagnostic).unwrap();
            assert!(!saved.contains(&private()), "{saved}");
        }
    }
}

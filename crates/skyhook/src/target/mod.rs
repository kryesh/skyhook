//! The session's target registry and routing.

mod config;
mod definition;
mod error;
mod options;
mod registry;
mod router;
mod ssh;
mod winrm;

pub use config::{Connection, ConnectionKind, Password, TargetConfig, TargetsConfig};
pub use definition::{
    Revisions, Route, RouteArguments, RouteHop, SshJump, TargetDefinition, TargetSource,
};
pub use error::{TargetEdge, TargetError};
pub use options::TargetField;
pub use ssh::{SshAuth, SshAuthKind, SshOptions};
pub use winrm::WinrmOptions;

pub(crate) use config::{ConfiguredPasswords, ConfiguredTargets, PasswordUnconfigured};
pub(crate) use options::SshOption;
pub(crate) use registry::{TargetRegistry, replaces};
pub(crate) use router::TargetRouter;

#[cfg(test)]
pub(crate) mod tests {
    use super::{Connection, SshOptions, TargetConfig, TargetDefinition};

    /// The configured SSH target `name`, its workspace `workspace`, jumping through `via`.
    pub(crate) fn definition(
        name: &str,
        workspace: impl Into<skyhook_worker::op::TargetPath>,
        via: Option<&str>,
    ) -> TargetDefinition {
        let config = TargetConfig {
            host: format!("{name}.example.com"),
            os: None,
            transport: Connection::Ssh(SshOptions::default()),
            workspace: workspace.into(),
            via: via.map(str::to_owned),
            origin: None,
        };
        TargetDefinition::from_config(name.to_owned(), config).unwrap()
    }

    /// The configured WinRM target `name`, reached from `origin`.
    pub(crate) fn winrm(name: &str, origin: Option<&str>) -> TargetDefinition {
        let config = serde_json::json!({
            "host": format!("{name}.example.com"), "origin": origin,
            "transport": {"winrm":
                {"user": "alice", "password": "pw", "auth": "negotiate", "tls": "system"}},
        });
        let config = serde_json::from_value(config).unwrap();
        TargetDefinition::from_config(name.to_owned(), config).unwrap()
    }
}

//! Typed execution targets, configuration, and routing.

mod config;
mod registry;
mod router;

pub use config::{
    SshOptions, TargetAuth, TargetConfig, TargetConfigType, TargetType, TargetsConfig,
};
pub use registry::{
    ROOT_TARGET, TargetDefinition, TargetError, TargetRecord, TargetRegistry, TargetSource,
};
pub(crate) use router::{ResolvedRoute, RouteIdentity, TargetRouter};

//! Named local and SSH execution targets.

mod config;
mod import;
mod registry;

pub use config::{TargetAuth, TargetConfig, TargetsConfig};
pub use registry::{
    ROOT_TARGET, TargetDefinition, TargetError, TargetRecord, TargetRegistry, TargetSource,
};

pub(crate) use import::import_ssh_targets;

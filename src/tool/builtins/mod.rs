//! Built-in coding, helper, and job-control tools.

mod fetch;
pub(crate) mod filesystem;
pub(crate) mod jobs;
mod process;
mod script;
pub(crate) mod search;
mod skills;
mod targets;
pub(crate) mod workspace;

use crate::{
    job::JobManager,
    session::SessionStore,
    tool::{RegistryError, ToolRegistryBuilder},
};

/// Builtin tool names, shared by registration and host presentation.
pub mod names {
    pub const AGENT: &str = "agent";
    pub const EXEC: &str = "exec";
    pub const READ: &str = "read";
    pub const REPLACE: &str = "replace";
    pub const SCRIPT: &str = "script";
    pub const WRITE: &str = "write";
}
pub(crate) use script::install_script_tool;
pub use skills::HostSkills;

/// Register the standard workspace, process, and job-control tool set.
pub(crate) fn register_coding_tools(
    builder: &mut ToolRegistryBuilder,
    store: SessionStore,
    jobs: JobManager,
    skills: HostSkills,
    router: crate::target::TargetRouter,
) -> Result<(), RegistryError> {
    builder.register_local(register_local_tools)?;
    jobs::register(builder, jobs)?;
    skills::register(builder, skills, store)?;
    targets::register(builder, router)?;
    Ok(())
}

/// An attached image as a tool result describes it; the image itself is attached.
#[derive(serde::Serialize, schemars::JsonSchema)]
struct ImageSummary {
    format: crate::media::ImageFormat,
    bytes: u64,
}

impl From<&crate::media::ImageRef> for ImageSummary {
    fn from(image: &crate::media::ImageRef) -> Self {
        Self {
            format: image.format,
            bytes: image.blob.bytes,
        }
    }
}

fn default_dot() -> String {
    crate::tool::registry::DEFAULT_PATH.to_owned()
}

use crate::newtype::Bounded;

/// Seconds a call may run.
type TimeoutSecs = Bounded<1, 3600>;

impl TimeoutSecs {
    fn duration(self) -> std::time::Duration {
        std::time::Duration::from_secs(self.into())
    }
}

pub(crate) fn register_local_tools(
    builder: &mut crate::tool::invocation::LocalCatalogBuilder,
) -> Result<(), RegistryError> {
    filesystem::register(builder)?;
    search::register(builder)?;
    process::register(builder, tokio::time::sleep)?;
    fetch::register(builder)?;
    Ok(())
}

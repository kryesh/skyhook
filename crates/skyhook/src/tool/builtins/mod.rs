//! Built-in host tools: job control, skills, targets and scripts, beside the target tools.

pub(crate) mod jobs;
mod script;
mod skills;

mod targets;

use crate::tool::catalog::RegistryError;
use crate::{job::JobManager, session::SessionStore, tool::ToolRegistryBuilder};

/// Host tool names, shared by registration and host presentation.
pub mod names {
    pub const AGENT: &str = "agent";
    pub const SCRIPT: &str = "script";
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
    crate::tool::target_tools::register(builder)?;
    jobs::register(builder, jobs)?;
    skills::register(builder, skills, store)?;
    targets::register(builder, router)?;
    Ok(())
}

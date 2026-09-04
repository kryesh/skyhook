//! Built-in coding, helper, and job-control tools.

mod filesystem;
mod jobs;
mod process;
mod script;
mod search;
mod skills;
mod targets;
pub(crate) mod workspace;

use crate::{
    job::JobManager,
    session::SessionStore,
    tool::{RegistryError, ToolRegistryBuilder},
};

pub use process::ProcessOutput;
pub use script::install_script_tool;
pub(crate) use script::install_script_tool_weak;
pub use skills::HostSkills;

/// Register the standard workspace, process, and job-control tool set.
pub(crate) fn register_coding_tools(
    builder: &mut ToolRegistryBuilder,
    store: SessionStore,
    jobs: JobManager,
    skills: HostSkills,
    router: crate::target::TargetRouter,
) -> Result<(), RegistryError> {
    filesystem::register(builder, store.clone())?;
    search::register(builder)?;
    process::register(builder)?;
    jobs::register(builder, jobs)?;
    skills::register(builder, skills)?;
    targets::register(builder, store, router)?;
    Ok(())
}

/// Register only tools whose effects are local to a worker workspace.
pub fn register_worker_tools(
    builder: &mut ToolRegistryBuilder,
    store: SessionStore,
    jobs: JobManager,
) -> Result<(), RegistryError> {
    filesystem::register(builder, store)?;
    search::register(builder)?;
    process::register(builder)?;
    jobs::register(builder, jobs)?;
    Ok(())
}

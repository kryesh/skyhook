//! Built-in coding, helper, and job-control tools.

mod filesystem;
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

pub use process::ProcessOutput;
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
    register_worker_tools(builder, store.clone())?;
    jobs::register(builder, jobs)?;
    skills::register(builder, skills)?;
    targets::register(builder, store, router)?;
    Ok(())
}

/// Register only tools whose effects are local to a worker workspace.
pub(crate) fn register_worker_tools(
    builder: &mut ToolRegistryBuilder,
    store: SessionStore,
) -> Result<(), RegistryError> {
    filesystem::register(builder, store)?;
    search::register(builder)?;
    process::register(builder)?;
    Ok(())
}

//! Built-in coding, helper, and job-control tools.

mod fetch;
mod fetch_text;
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
    builder.register_local(register_local_tools)?;
    jobs::register(builder, jobs)?;
    skills::register(builder, skills, store.clone())?;
    targets::register(builder, store, router)?;
    Ok(())
}

fn default_dot() -> String {
    ".".to_owned()
}

pub(crate) fn register_local_tools(
    builder: &mut crate::tool::invocation::LocalCatalogBuilder,
) -> Result<(), RegistryError> {
    filesystem::register(builder)?;
    search::register(builder)?;
    process::register(builder)?;
    fetch::register(builder)?;
    Ok(())
}

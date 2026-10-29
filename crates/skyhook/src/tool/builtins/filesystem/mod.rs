//! Workspace filesystem tools.
use crate::tool::invocation::LocalCatalogBuilder;

mod mutations;
mod read;

use crate::tool::RegistryError;

pub(crate) fn register(builder: &mut LocalCatalogBuilder) -> Result<(), RegistryError> {
    read::register(builder)?;
    mutations::register(builder)
}

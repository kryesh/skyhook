//! The configuration files the applications read: their discovery where a command line
//! says to find them, the layering of user and workspace files, and what both hand a
//! launch as values.

mod layer;
mod layers;
mod loader;

pub use loader::{LayerResolutionError, LayeredConfig, ResolvedConfig, resolve_layers};

use crate::args::{ConfigArgs, ProductArgs};
use skyhook::launch::{
    self, ConfiguredHost, LaunchError, config::HostConfig, request::ExecutionRequest,
};
use skyhook::product::{Attendance, Product};
use skyhook::tool::policy::HostCapabilityDeclaration;
use std::path::Path;

/// The main configuration with `request`'s policy, and `args` read into the product's
/// options for `workspace`. The report covers both files, the product's last.
pub(crate) async fn load<O>(
    request: &ConfigArgs,
    args: impl ProductArgs<O>,
    workspace: &Path,
    declared: &[HostCapabilityDeclaration],
) -> Result<(ResolvedConfig<HostConfig>, O), LayerResolutionError> {
    let source = &request.source;
    let explicit = source.config.as_deref();
    let resolved = resolve_layers::<HostConfig>(&source.workspace, explicit, Some(declared));
    let mut resolved = resolved.await?;
    resolved.config.approve_all |= request.approve_all;
    let product = args.load(workspace).await?;
    resolved.report.sources.extend(product.report.sources);
    let diagnostics = &mut resolved.report.diagnostics;
    diagnostics.extend(product.report.diagnostics);
    Ok((resolved, product.config))
}

/// Why a product could not be configured for a launch.
#[derive(Debug, thiserror::Error)]
pub enum ConfigureError {
    #[error(transparent)]
    Files(#[from] LayerResolutionError),
    #[error(transparent)]
    Launch(#[from] LaunchError),
}

/// Configure `product` for the sessions `attendance` opens as `execution` asks, from the
/// files `request` names. Beside the launch is what the files' loaders and the product
/// warned of, for the caller to show before anything refuses the launch.
pub(crate) async fn configure<P: Product>(
    product: &P,
    args: impl ProductArgs<P::Options>,
    request: &ConfigArgs,
    execution: &ExecutionRequest,
    attendance: Attendance,
) -> Result<(ConfiguredHost, Vec<String>), ConfigureError> {
    // A workspace that cannot be resolved is refused before any file is read.
    let workspace = tokio::fs::canonicalize(&request.source.workspace)
        .await
        .map_err(LaunchError::Workspace)?;
    let declared = product.identity().capabilities;
    let (resolved, options) = load(request, args, &workspace, &declared).await?;
    let host = resolved.config;
    let mut host =
        launch::configure(product, options, host, &workspace, execution, attendance).await?;
    let warnings = (resolved.report.diagnostics.iter().map(ToString::to_string))
        .chain(std::mem::take(&mut host.warnings))
        .collect();
    Ok((host, warnings))
}

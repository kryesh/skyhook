//! Host-owned assets are sent to the caller's workspace, not the host tool's workspace.
use base64::Engine as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::workspace::{atomic_write, resolve_for_authorization, resolve_writable};
use crate::{
    target::TargetRouter,
    tool::{
        PathKind, RegistryError, ToolContext, ToolError, ToolOptions, ToolPlacement,
        ToolRegistryBuilder,
        policy::{ApprovalGrant, Capability, PathAccess, PermissionUse, ResourceId},
    },
};

const COPY_TOOL: &str = "__skill_copy";
const MAX_COPY_BYTES: usize = 8 * 1024 * 1024;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CopyArgs {
    to: String,
    data_base64: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct CopyOutput {
    to: String,
}

/// Only installed in remote workers, never in the host/model/script registry.
pub(crate) fn register_worker(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    builder.register::<CopyArgs, CopyOutput, _, _>(
        COPY_TOOL,
        "Receive a host-owned skill asset.",
        ToolOptions::new(vec![Capability::Write])
            .script_only()
            .script_unavailable()
            .placement(ToolPlacement::InheritWorkspace)
            .path_argument("to", PathAccess::Write, PathKind::Writable),
        |context, args| async move {
            if args.data_base64.len() > MAX_COPY_BYTES.div_ceil(3) * 4 {
                return Err(ToolError::InvalidArguments(
                    "skill asset is too large".into(),
                ));
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(args.data_base64)
                .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
            if bytes.len() > MAX_COPY_BYTES {
                return Err(ToolError::InvalidArguments(
                    "skill asset is too large".into(),
                ));
            }
            let to = write(&context.execution_location.workspace, &args.to, &bytes).await?;
            Ok(CopyOutput { to })
        },
    )?;
    Ok(())
}

async fn write(workspace: &std::path::Path, to: &str, bytes: &[u8]) -> Result<String, ToolError> {
    let path = resolve_writable(workspace, to).await?;
    if tokio::fs::metadata(&path)
        .await
        .is_ok_and(|metadata| metadata.is_dir())
    {
        return Err(ToolError::InvalidArguments(
            "to must name a file, not a directory".into(),
        ));
    }
    atomic_write(&path, bytes).await?;
    Ok(path.to_string_lossy().into_owned())
}

pub(super) async fn copy(
    router: &TargetRouter,
    context: &ToolContext,
    to: &str,
    bytes: &[u8],
) -> Result<String, ToolError> {
    let caller = &context.caller_location;
    if caller.is_root() {
        let resolved = resolve_for_authorization(&caller.workspace, to, PathKind::Writable).await?;
        let mut permissions = vec![PermissionUse::new(
            Capability::Write,
            ResourceId::workspace(&caller.target, &caller.workspace),
        )];
        // Host placement retains the session's authorization root even for a child workspace.
        if !resolved
            .path
            .starts_with(&context.execution_location.workspace)
        {
            let resource = ResourceId::path(&caller.target, &resolved.path);
            let grant = if resolved.directory {
                ApprovalGrant::descendants(Capability::Write, resource.clone())
            } else {
                ApprovalGrant::exact(Capability::Write, resource.clone())
            };
            permissions.push(PermissionUse::new(Capability::Write, resource).with_grant(grant));
        }
        router
            .authorize_transfer(
                context,
                "skill",
                permissions,
                serde_json::json!({"to": resolved.path}),
            )
            .await?;
        return write(&caller.workspace, &resolved.path.to_string_lossy(), bytes).await;
    }
    router
        .authorize_transfer(
            context,
            "skill",
            vec![PermissionUse::new(
                Capability::Write,
                ResourceId::workspace(&caller.target, &caller.workspace),
            )],
            serde_json::json!({"to": to}),
        )
        .await?;
    let route = router
        .resolve(&caller.target)
        .await
        .map_err(|error| ToolError::Failed(error.to_string()))?;
    router
        .authorize_transfer(
            context,
            "connect_target",
            vec![route.permission()],
            route.authorization_arguments(),
        )
        .await?;
    let connection = router
        .prepare(route, &caller.workspace, &context.authorization)
        .await
        .map_err(|error| ToolError::Failed(error.to_string()))?;
    // The worker resolves `to` and requests path permissions on its own filesystem.
    // Base64 keeps the maximum 8 MiB asset safely below the protocol's 16 MiB frame limit.
    let result = connection
        .execute(
            COPY_TOOL.into(),
            serde_json::json!({
                "to": to,
                "data_base64": base64::engine::general_purpose::STANDARD.encode(bytes),
            }),
            context,
        )
        .await
        .map_err(|error| ToolError::Failed(error.to_string()))?;
    Ok(serde_json::from_value::<CopyOutput>(result.value)?.to)
}

#[cfg(test)]
#[path = "skill_transfer_tests.rs"]
mod tests;

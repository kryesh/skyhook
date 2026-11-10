//! Admission: the permissions a call needs, its paths resolved on the target it
//! runs on.
use crate::tool::policy::{Capability, CapabilitySet, PermissionUse, ResourceId};
use skyhook_tools::target::ExecutionLocation;
use skyhook_tools::target::TargetRef;
use skyhook_tools::{
    error::AdmissionError,
    path::{JobWorkspace, PathAccess, PathArgument, PathNeed, PathScope},
};
use skyhook_worker::diagnostic::PathFact;
use skyhook_worker::{
    op::{Resolved, TargetPath},
    target::Target,
};

use crate::tool::catalog::RegisteredTool;

/// The permissions an invocation needs, wherever it is planned. Permissions its
/// arguments derive replace their capabilities' static scope, as do its path
/// permissions; the capabilities left are scoped to the tool's resource or
/// workspace. A capability this context lacks makes the tool unavailable, not denied.
pub(crate) fn assemble_permissions(
    tool: &RegisteredTool,
    (target, workspace): (&TargetRef, &TargetPath),
    available: &CapabilitySet,
    arguments: Vec<PermissionUse>,
    path: &PathPreflight,
) -> Result<Vec<PermissionUse>, AdmissionError> {
    let mut covered = arguments.iter().collect::<Vec<_>>();
    covered.extend(&path.permissions);
    let mut capabilities = tool.capabilities();
    capabilities.retain(|capability| {
        !covered
            .iter()
            .any(|permission| permission.capability == *capability)
    });
    let resource = tool.permission_resource();
    let mut permissions = scope_capabilities(capabilities, target, workspace, resource);
    permissions.extend(path.permissions.iter().cloned());
    permissions.extend(arguments);
    if (permissions.iter()).any(|permission| !available.contains(&permission.capability)) {
        return Err(AdmissionError::unavailable(tool.name()));
    }
    Ok(permissions)
}

/// Resolve `arguments` where the invocation runs, in one request, and replace each
/// with its resolved spelling.
pub(crate) async fn admit_paths<T>(
    target: &T,
    arguments: Vec<PathArgument<'_>>,
    location: &ExecutionLocation,
) -> Result<PathPreflight, AdmissionError>
where
    T: Target<Unreachable: Into<AdmissionError>>,
{
    let Some(request) = skyhook_tools::path::request(&arguments, location) else {
        return Ok(PathPreflight::default());
    };
    let resolved = match target.call(request).await.map_err(Into::into)? {
        Ok(resolved) => resolved,
        Err(never) => match never {},
    };
    preflight(arguments, resolved, location)
}

/// What `arguments` need, as their target `resolved` them, each replaced with its
/// resolved spelling.
pub(crate) fn preflight(
    arguments: Vec<PathArgument<'_>>,
    resolved: Vec<Resolved>,
    location: &ExecutionLocation,
) -> Result<PathPreflight, AdmissionError> {
    let admitted = skyhook_tools::path::admit(arguments, resolved, location)?;
    Ok(PathPreflight {
        permissions: (admitted.needs.iter())
            .filter_map(|need| permission(need, &location.target))
            .collect(),
        paths: admitted.facts,
        unresolved: admitted.unresolved,
        found: admitted.found,
    })
}

/// Scope capabilities to `resource`, or else `target`'s `workspace`.
pub(crate) fn scope_capabilities(
    capabilities: Vec<Capability>,
    target: &TargetRef,
    workspace: &TargetPath,
    resource: Option<&ResourceId>,
) -> Vec<PermissionUse> {
    let resource = match resource {
        Some(resource) => resource.clone(),
        None => ResourceId::workspace(target, workspace),
    };
    (capabilities.into_iter())
        .map(|capability| PermissionUse::new(capability, resource.clone()))
        .collect()
}

/// A call's paths as its target resolved them, and what they need.
#[derive(Default)]
pub(crate) struct PathPreflight {
    pub permissions: Vec<PermissionUse>,
    pub paths: Vec<PathFact>,
    /// How the first path outside the root that does not resolve fails, once
    /// the call is authorized without asking anyone.
    pub unresolved: Option<AdmissionError>,
    /// Whether one names a job's missing workspace.
    pub found: JobWorkspace,
}

/// The permission `resolved` needs beyond the authorization root, which its caller
/// has already authorized as a whole, unless its `scope` always needs one. It
/// proposes a grant over a directory's descendants or the exact file.
fn permission(need: &PathNeed, target: &TargetRef) -> Option<PermissionUse> {
    let PathNeed {
        access,
        scope,
        path: resolved,
    } = need;
    let capability = access_capability(*access);
    if !resolved.outside && *scope == PathScope::Workspace {
        return None;
    }
    let resource = ResourceId::Path {
        target: target.clone(),
        components: resolved.components.clone(),
    };
    Some(if resolved.directory {
        PermissionUse::descendants(capability, resource)
    } else {
        PermissionUse::exact(capability, resource)
    })
}

/// The capability an access to a path needs.
pub(crate) const fn access_capability(access: PathAccess) -> Capability {
    match access {
        PathAccess::Read => Capability::Read,
        PathAccess::Write => Capability::Write,
    }
}

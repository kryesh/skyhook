//! Validate and rebase remote authorization resources at the host boundary.
use crate::remote::error::{ProtocolError, RemoteError};
use crate::target::TargetRef;
use crate::tool::policy::PermissionUse;

/// A worker scopes permissions to its own machine, which the host knows as
/// `target`. A forwarded permission's proposed grant is derived from its
/// resource, so rebasing the resource rebases the proposal with it.
pub(super) fn rebase_remote_permissions(
    target: &TargetRef,
    permissions: &mut [PermissionUse],
) -> Result<(), RemoteError> {
    for permission in permissions {
        let Some(origin) = permission.resource.execution_target_mut() else {
            continue;
        };
        if *origin != TargetRef::Root {
            return Err(RemoteError::Protocol(
                ProtocolError::UnexpectedPermissionTarget(origin.clone()),
            ));
        }
        origin.clone_from(target);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::policy::{Capability, PathText, ResourceId};

    #[test]
    fn forwarded_permissions_and_grants_only_rebase_execution_targets() {
        let (outside, workspace) = (
            PathText::new("/outside").unwrap(),
            PathText::new("/workspace").unwrap(),
        );
        let origin = "https://example.test:8443";
        let build: TargetRef = "build".parse().unwrap();
        let mut cases = vec![
            (
                Capability::Read,
                ResourceId::path(&TargetRef::Root, &outside),
                ResourceId::path(&build, &outside),
            ),
            (
                Capability::Write,
                ResourceId::workspace(&TargetRef::Root, &workspace),
                ResourceId::workspace(&build, &workspace),
            ),
            (
                Capability::Network,
                ResourceId::network(&TargetRef::Root, origin),
                ResourceId::network(&build, origin),
            ),
        ];
        // Routes, sessions and MCP tools are not execution targets.
        let jump: crate::target::TargetName = "jump".parse().unwrap();
        for (capability, resource) in [
            (
                Capability::Targets,
                ResourceId::route(jump.clone(), vec![(jump, 1)]),
            ),
            (Capability::Read, ResourceId::session("root")),
            (Capability::Mcp, ResourceId::mcp("root", "tool")),
        ] {
            cases.push((capability, resource.clone(), resource));
        }
        for (capability, resource, expected) in cases {
            let mut permissions = vec![PermissionUse::descendants(capability, resource)];
            rebase_remote_permissions(&build, &mut permissions).unwrap();
            assert_eq!(permissions[0].resource, expected);
            let grant = permissions[0].proposed_grant().unwrap();
            assert_eq!(grant.resource, expected);
        }
    }

    #[test]
    fn forwarded_permissions_cannot_claim_another_target() {
        let build: TargetRef = "build".parse().unwrap();
        for resource in [
            ResourceId::path(
                &"other".parse().unwrap(),
                &PathText::new("/outside").unwrap(),
            ),
            ResourceId::workspace(
                &"other".parse().unwrap(),
                &PathText::new("/workspace").unwrap(),
            ),
            ResourceId::network(&"other".parse().unwrap(), "https://example.test"),
        ] {
            let result = rebase_remote_permissions(
                &build,
                &mut [PermissionUse::new(Capability::Read, resource)],
            );
            assert!(matches!(result, Err(RemoteError::Protocol(_))));
        }
    }
}

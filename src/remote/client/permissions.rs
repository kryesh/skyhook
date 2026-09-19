//! Validate and rebase remote authorization resources at the host boundary.
use crate::remote::RemoteError;
use crate::tool::policy::PermissionUse;

pub(super) fn rebase_remote_permissions(
    target: &str,
    permissions: &mut [PermissionUse],
) -> Result<(), RemoteError> {
    fn rebase_resource(
        target: &str,
        resource: &mut crate::tool::policy::ResourceId,
    ) -> Result<(), RemoteError> {
        let Some(origin) = resource.execution_target_mut() else {
            return Ok(());
        };
        if origin != "root" {
            return Err(RemoteError::Protocol(format!(
                "remote scoped permission used unexpected execution target `{origin}`"
            )));
        }
        target.clone_into(origin);
        Ok(())
    }

    for permission in permissions {
        rebase_resource(target, &mut permission.resource)?;
        if let Some(grant) = &mut permission.proposed_grant {
            rebase_resource(target, &mut grant.resource)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::policy::{ApprovalGrant, Capability, ResourceId};
    use std::path::Path;

    #[test]
    fn forwarded_permissions_and_grants_only_rebase_execution_targets() {
        let (outside, workspace) = (Path::new("/outside"), Path::new("/workspace"));
        let origin = "https://example.test:8443";
        let mut cases = vec![
            (
                Capability::Read,
                ResourceId::path("root", outside),
                ResourceId::path("build", outside),
            ),
            (
                Capability::Write,
                ResourceId::workspace("root", workspace),
                ResourceId::workspace("build", workspace),
            ),
            (
                Capability::Network,
                ResourceId::network("root", origin),
                ResourceId::network("build", origin),
            ),
        ];
        // Routes, sessions and MCP tools are not execution targets.
        for (capability, resource) in [
            (
                Capability::Targets,
                ResourceId::route("root", vec![("root".into(), 1)]),
            ),
            (Capability::Read, ResourceId::session("root")),
            (Capability::Mcp, ResourceId::mcp("root", "tool")),
        ] {
            cases.push((capability, resource.clone(), resource));
        }
        for (capability, resource, expected) in cases {
            let grant = ApprovalGrant::descendants(capability, resource.clone());
            let mut permissions = vec![PermissionUse::new(capability, resource).with_grant(grant)];
            rebase_remote_permissions("build", &mut permissions).unwrap();
            assert_eq!(permissions[0].resource, expected);
            let grant = permissions[0].proposed_grant.as_ref().unwrap();
            assert_eq!(grant.resource, expected);
        }
    }

    #[test]
    fn forwarded_permissions_and_grants_cannot_claim_another_target() {
        for resource in [
            ResourceId::path("other", Path::new("/outside")),
            ResourceId::workspace("other", Path::new("/workspace")),
            ResourceId::network("other", "https://example.test"),
        ] {
            let grant = ApprovalGrant::exact(Capability::Read, resource.clone());
            let session = ResourceId::session("test");
            for permission in [
                PermissionUse::new(Capability::Read, resource),
                PermissionUse::new(Capability::Read, session).with_grant(grant),
            ] {
                let result = rebase_remote_permissions("build", &mut [permission]);
                assert!(matches!(result, Err(RemoteError::Protocol(_))));
            }
        }
    }
}

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
        if !matches!(
            resource.namespace.as_str(),
            "path" | "network" | "workspace"
        ) {
            return Ok(());
        }
        let Some(origin) = resource.segments.first_mut() else {
            return Err(RemoteError::Protocol(
                "remote scoped permission omitted its execution target".to_owned(),
            ));
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
    #[test]
    fn forwarded_path_permissions_are_rebased_to_the_destination() {
        let path = ResourceId::new("path", ["root", "/", "outside"]);
        let mut permissions = vec![
            PermissionUse::new(Capability::Write, path.clone())
                .with_grant(ApprovalGrant::descendants(Capability::Write, path)),
        ];

        rebase_remote_permissions("build", &mut permissions).unwrap();

        assert_eq!(permissions[0].resource.segments[0], "build");
        assert_eq!(
            permissions[0]
                .proposed_grant
                .as_ref()
                .unwrap()
                .resource
                .segments[0],
            "build"
        );
    }

    #[test]
    fn forwarded_network_permissions_preserve_origins_and_rebase_targets() {
        let mut permissions = vec![PermissionUse::new(
            Capability::Network,
            ResourceId::network("root", "https://example.test:8443"),
        )];
        rebase_remote_permissions("build", &mut permissions).unwrap();
        assert_eq!(
            permissions[0].resource,
            ResourceId::network("build", "https://example.test:8443")
        );
        assert!(permissions[0].proposed_grant.is_none());
        let mut forged = vec![PermissionUse::new(
            Capability::Network,
            ResourceId::network("another-target", "https://example.test"),
        )];
        assert!(matches!(
            rebase_remote_permissions("build", &mut forged),
            Err(RemoteError::Protocol(_))
        ));
    }

    #[test]
    fn forwarded_path_permissions_cannot_claim_another_target() {
        let mut permissions = vec![PermissionUse::new(
            Capability::Read,
            ResourceId::new("path", ["other", "/", "outside"]),
        )];
        assert!(matches!(
            rebase_remote_permissions("build", &mut permissions),
            Err(RemoteError::Protocol(_))
        ));
    }

    #[test]
    fn forwarded_network_permissions_and_grants_are_target_scoped() {
        let resource = ResourceId::network("root", "https://example.test:8443");
        let mut permissions = vec![
            PermissionUse::new(Capability::Network, resource.clone())
                .with_grant(ApprovalGrant::exact(Capability::Network, resource)),
        ];
        rebase_remote_permissions("build", &mut permissions).unwrap();
        let expected = ResourceId::network("build", "https://example.test:8443");
        assert_eq!(permissions[0].resource, expected);
        assert_eq!(
            permissions[0].proposed_grant.as_ref().unwrap().resource,
            expected
        );

        for resource in [
            ResourceId::network("other", "https://example.test"),
            ResourceId::new("network", std::iter::empty::<String>()),
        ] {
            let mut permissions = vec![PermissionUse::new(Capability::Network, resource)];
            assert!(matches!(
                rebase_remote_permissions("build", &mut permissions),
                Err(RemoteError::Protocol(_))
            ));
        }
        let mut permissions = vec![
            PermissionUse::new(
                Capability::Network,
                ResourceId::network("root", "https://example.test"),
            )
            .with_grant(ApprovalGrant::exact(
                Capability::Network,
                ResourceId::network("other", "https://example.test"),
            )),
        ];
        assert!(matches!(
            rebase_remote_permissions("build", &mut permissions),
            Err(RemoteError::Protocol(_))
        ));
    }
}

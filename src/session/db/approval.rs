//! Approvals: grants of one resource each, and their revocations.

use super::{decode::*, *};
use crate::{
    session::SessionEvent,
    tool::policy::{ApprovalGrant, ResourceId, ResourceKind},
};

impl Encoder {
    pub(super) fn grant(&mut self, db: &Db, seq: u64, grant: &ApprovalGrant) -> DbResult<()> {
        let target = match &grant.resource {
            ResourceId::Workspace { target, .. }
            | ResourceId::Path { target, .. }
            | ResourceId::Network { target, .. } => Some(self.target(db, target)?),
            ResourceId::Route { destination, .. } => {
                Some(self.target(db, &destination.clone().into())?)
            }
            ResourceId::Session { .. } | ResourceId::Mcp { .. } => None,
        };
        let (path, origin, session, server, tool) = match &grant.resource {
            ResourceId::Workspace { path, .. } => (Some(path), None, None, None, None),
            ResourceId::Network { origin, .. } => (None, Some(origin), None, None, None),
            ResourceId::Session { name } => (None, None, Some(name), None, None),
            ResourceId::Mcp { server, tool } => (None, None, None, Some(server), Some(tool)),
            ResourceId::Path { .. } | ResourceId::Route { .. } => (None, None, None, None, None),
        };
        db.execute(
            "INSERT INTO approval_grant (entry, capability, resource_kind, target, path, \
             origin, session_name, mcp_server, mcp_tool, coverage) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                seq,
                grant.capability,
                grant.resource.kind(),
                target,
                path,
                origin,
                session,
                server,
                tool,
                grant.coverage
            ],
        )?;
        match &grant.resource {
            ResourceId::Path { components, .. } => {
                for (position, component) in components.iter().enumerate() {
                    db.execute(
                        "INSERT INTO approval_grant_path_component (grant_entry, position, \
                         component) VALUES (?1, ?2, ?3)",
                        params![seq, position, component],
                    )?;
                }
            }
            ResourceId::Route { hops, .. } => {
                for (position, (name, revision)) in hops.iter().enumerate() {
                    let target = self.target(db, &name.clone().into())?;
                    db.execute(
                        "INSERT INTO approval_grant_route_hop (grant_entry, position, target, \
                         revision) VALUES (?1, ?2, ?3, ?4)",
                        params![seq, position, target, *revision],
                    )?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Whether a resource of `kind` names a target.
pub(super) fn targeted(kind: ResourceKind) -> bool {
    !matches!(kind, ResourceKind::Session | ResourceKind::Mcp)
}

/// Grants and revocations.
pub(super) fn events(events: &mut Events) -> DbResult<()> {
    let db = events.db;
    let path_components = grouped(
        db,
        "SELECT grant_entry, component FROM approval_grant_path_component \
         ORDER BY grant_entry, position",
        |row| Ok(row.get::<String>(1)?),
    )?;
    let route_hops = grouped(
        db,
        "SELECT h.grant_entry, t.name, h.revision FROM approval_grant_route_hop h \
         JOIN target t ON t.id = h.target ORDER BY h.grant_entry, h.position",
        |row| Ok((parsed(row.get::<String>(1)?)?, row.get::<u64>(2)?)),
    )?;
    events.load(
        "SELECT g.entry, g.capability, g.resource_kind, t.name, g.path, g.origin, \
         g.session_name, g.mcp_server, g.mcp_tool, g.coverage FROM approval_grant g \
         LEFT JOIN target t ON t.id = g.target",
        |row| {
            let seq = row.get::<i64>(0)?;
            let text = |index: i32| -> DbResult<String> {
                row.get::<Option<String>>(index)?
                    .ok_or_else(|| corrupt("approval grant resource column is missing"))
            };
            let resource = match enum_column(row, 2)? {
                ResourceKind::Workspace => ResourceId::Workspace {
                    target: parsed(text(3)?)?,
                    path: text(4)?,
                },
                ResourceKind::Path => ResourceId::Path {
                    target: parsed(text(3)?)?,
                    components: path_components.get(&seq).cloned().unwrap_or_default(),
                },
                ResourceKind::Network => ResourceId::Network {
                    target: parsed(text(3)?)?,
                    origin: text(5)?,
                },
                ResourceKind::Route => ResourceId::Route {
                    destination: parsed(text(3)?)?,
                    hops: route_hops.get(&seq).cloned().unwrap_or_default(),
                },
                ResourceKind::Session => ResourceId::Session { name: text(6)? },
                ResourceKind::Mcp => ResourceId::Mcp {
                    server: text(7)?,
                    tool: text(8)?,
                },
            };
            Ok(SessionEvent::ApprovalGranted {
                grant: ApprovalGrant {
                    capability: enum_column(row, 1)?,
                    resource,
                    coverage: enum_column(row, 9)?,
                },
            })
        },
    )?;
    events.load(
        "SELECT entry, grant_entry FROM approval_revocation",
        |row| {
            Ok(SessionEvent::ApprovalRevoked {
                grant: sequence(row.get(1)?),
            })
        },
    )
}

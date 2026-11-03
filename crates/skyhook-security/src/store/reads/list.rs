//! Keyset listings of live nodes of one kind.

use schemars::JsonSchema;
use serde::Serialize;
use skyhook::db::{optional_enum_column, params};

use super::super::*;

/// What a listing of one kind of node selects.
#[derive(Clone, Copy, Debug)]
pub enum ListQuery {
    Component,
    Path,
    State {
        scope: Option<StateScope>,
    },
    Premise {
        scope: Option<PremiseScope>,
        status: Option<PremiseStatus>,
    },
    Flaw {
        status: Option<FlawStatus>,
        component: Option<ComponentId>,
    },
    Candidate {
        root: Option<FlawId>,
        state: Option<Status>,
    },
    Finding,
}

impl ListQuery {
    const fn kind(self) -> NodeKind {
        match self {
            Self::Component => NodeKind::Component,
            Self::Path => NodeKind::Path,
            Self::State { .. } => NodeKind::State,
            Self::Premise { .. } => NodeKind::Premise,
            Self::Flaw { .. } => NodeKind::Flaw,
            Self::Candidate { .. } => NodeKind::Candidate,
            Self::Finding => NodeKind::Finding,
        }
    }
}

/// A node's own status, by its kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum NodeStatus {
    Inventory(InventoryStatus),
    Path(PathStatus),
    Premise(PremiseStatus),
    Flaw(FlawStatus),
    Candidate(Status),
}

/// A live node as a listing shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct NodeSummary {
    pub id: NodeId,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<NodeStatus>,
}

impl Keyed for NodeSummary {
    fn key(&self) -> i64 {
        self.id.0
    }
}

/// The node of `kind` in a row of its id, title and own status.
pub(super) fn summary(row: &libsql::Row, kind: NodeKind) -> Result<NodeSummary> {
    let status = match kind {
        NodeKind::Component | NodeKind::State => {
            optional_enum_column(row, 2)?.map(NodeStatus::Inventory)
        }
        NodeKind::Path => optional_enum_column(row, 2)?.map(NodeStatus::Path),
        NodeKind::Premise => optional_enum_column(row, 2)?.map(NodeStatus::Premise),
        NodeKind::Flaw => optional_enum_column(row, 2)?.map(NodeStatus::Flaw),
        NodeKind::Candidate => optional_enum_column(row, 2)?.map(NodeStatus::Candidate),
        NodeKind::Finding => None,
    };
    Ok(NodeSummary {
        id: NodeId(row.get(0)?),
        title: row.get(1)?,
        status,
    })
}

/// Live nodes read as `n` after key `?1`, of kind `?2`, with `$status` and from `$from` where
/// `$filter` holds.
macro_rules! list_sql {
    ($status:literal, $from:literal, $filter:literal) => {
        concat!(
            "SELECT n.id,n.title,",
            $status,
            " FROM node n ",
            $from,
            " WHERE n.kind=?2 AND n.merged_into IS NULL AND n.id>?1 AND ",
            $filter,
            " ORDER BY n.id"
        )
    };
}

impl Store {
    /// A page of live nodes of one kind, oldest first.
    pub fn list(&self, query: &ListQuery, page: Page) -> Result<Listing<NodeSummary>> {
        let kind = query.kind();
        let mut bound = params![page.after, kind];
        let sql = match *query {
            ListQuery::Component => {
                list_sql!("c.status", "JOIN component c ON c.node=n.id", "1")
            }
            ListQuery::Path => list_sql!("p.status", "JOIN path p ON p.node=n.id", "1"),
            ListQuery::State { scope } => {
                bound.extend(params![scope]);
                list_sql!(
                    "s.status",
                    "JOIN state s ON s.node=n.id",
                    "(?3 IS NULL OR s.scope=?3)"
                )
            }
            ListQuery::Finding => list_sql!("NULL", "", "1"),
            ListQuery::Premise { scope, status } => {
                bound.extend(params![scope, status]);
                list_sql!(
                    "p.status",
                    "JOIN premise_standing p ON p.node=n.id",
                    "(?3 IS NULL OR p.scope=?3) AND (?4 IS NULL OR p.status=?4)"
                )
            }
            ListQuery::Flaw { status, component } => {
                bound.extend(params![status, component]);
                match component {
                    Some(_) => {
                        "SELECT n.id,n.title,f.status FROM flaw x CROSS JOIN node n ON n.id=x.node \
                         JOIN flaw_standing f ON f.node=n.id WHERE x.component=?4 AND x.node>?1 AND \
                         n.merged_into IS NULL AND (?3 IS NULL OR f.status=?3) ORDER BY x.node"
                    }
                    None => list_sql!(
                        "f.status",
                        "JOIN flaw_standing f ON f.node=n.id",
                        "(?3 IS NULL OR f.status=?3)"
                    ),
                }
            }
            ListQuery::Candidate { root, state } => {
                bound.extend(params![root, state]);
                match root {
                    Some(_) => {
                        "SELECT n.id,n.title,s.state FROM candidate_flaw r CROSS JOIN node n ON \
                         n.id=r.candidate JOIN candidate_state s ON s.candidate=n.id WHERE \
                         r.flaw=?3 AND r.position=0 AND r.candidate>?1 AND n.merged_into IS NULL \
                         AND (?4 IS NULL OR s.state=?4) ORDER BY r.candidate"
                    }
                    None => list_sql!(
                        "s.state",
                        "JOIN candidate_state s ON s.candidate=n.id",
                        "(?4 IS NULL OR s.state=?4)"
                    ),
                }
            }
        };
        self.read(|db| {
            let items = first_rows(db, sql, bound, page.limit(), |row| summary(row, kind))?;
            Ok(Listing::keyset(items, page))
        })
    }
}

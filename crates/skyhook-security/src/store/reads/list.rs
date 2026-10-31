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
    Source,
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
            Self::Source => NodeKind::Source,
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

/// Live nodes read as `n` after key `?1`, limit `?2`, with `$status` and from `$from` where
/// `$filter` holds.
macro_rules! list_sql {
    ($status:literal, $from:literal, $filter:literal) => {
        concat!(
            "SELECT n.id,n.title,",
            $status,
            " FROM node n ",
            $from,
            " WHERE n.kind=?3 AND n.merged_into IS NULL AND n.id>?1 AND ",
            $filter,
            " ORDER BY n.id LIMIT ?2"
        )
    };
}

impl Store {
    /// A page of live nodes of one kind, oldest first.
    pub fn list(&self, query: &ListQuery, page: Page) -> Result<Listing<NodeSummary>> {
        let kind = query.kind();
        let mut bound = params![page.after, page.limit(), kind];
        let sql = match *query {
            ListQuery::Component | ListQuery::Path | ListQuery::Source | ListQuery::Finding => {
                list_sql!("NULL", "", "1")
            }
            ListQuery::Premise { scope, status } => {
                bound.extend(params![scope, status]);
                list_sql!(
                    "p.status",
                    "JOIN premise_state p ON p.node=n.id",
                    "(?4 IS NULL OR p.scope=?4) AND (?5 IS NULL OR p.status=?5)"
                )
            }
            ListQuery::Flaw { status, component } => {
                bound.extend(params![status, component]);
                list_sql!(
                    "f.status",
                    "JOIN flaw_state f ON f.node=n.id",
                    "(?4 IS NULL OR f.status=?4) AND (?5 IS NULL OR f.component=?5)"
                )
            }
            ListQuery::Candidate { root, state } => {
                bound.extend(params![root, state]);
                list_sql!(
                    "s.state",
                    "JOIN candidate_state s ON s.candidate=n.id",
                    "(?4 IS NULL OR s.root=?4) AND (?5 IS NULL OR s.state=?5)"
                )
            }
        };
        self.read(|db| {
            let items = rows(db, sql, bound, |row| {
                let status = match kind {
                    NodeKind::Premise => optional_enum_column(row, 2)?.map(NodeStatus::Premise),
                    NodeKind::Flaw => optional_enum_column(row, 2)?.map(NodeStatus::Flaw),
                    NodeKind::Candidate => optional_enum_column(row, 2)?.map(NodeStatus::Candidate),
                    _ => None,
                };
                Ok(NodeSummary {
                    id: NodeId(row.get(0)?),
                    title: row.get(1)?,
                    status,
                })
            })?;
            Ok(Listing::keyset(items, page))
        })
    }
}

//! Keyset listings of one kind of record: live nodes, environments or tasks.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::db::{optional_enum_column, params};
use skyhook_base::bounded::Bounded;

use super::super::*;

// Records of one kind, filtered: live nodes, environments or tasks.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ListQuery {
    Component {},
    Path {},
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
    Finding {},
    Environment {},
    Task {},
}

/// A listed record: a node's summary, an environment or a task.
#[derive(Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(rename = "Listed")]
pub(crate) enum ListItem {
    Node(NodeSummary),
    Environment(EnvironmentView),
    Task(Task),
}

impl Keyed for ListItem {
    fn key(&self) -> i64 {
        match self {
            Self::Node(node) => node.key(),
            Self::Environment(environment) => environment.key(),
            Self::Task(task) => task.key(),
        }
    }
}

/// A node's own status, by its kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(untagged)]
pub(crate) enum NodeStatus {
    Inventory(InventoryStatus),
    Path(PathStatus),
    Premise(PremiseStatus),
    Flaw(FlawStatus),
    Candidate(Status),
}

/// A live node as a listing shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct NodeSummary {
    pub(crate) id: NodeId,
    pub(crate) title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<NodeStatus>,
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
    /// A page of records of one kind, oldest first.
    pub(crate) fn list(&self, query: &ListQuery, page: Page) -> Result<Listing<ListItem>> {
        let (kind, filter, sql) = match *query {
            ListQuery::Environment {} => {
                return Ok(self.environments(page)?.map(ListItem::Environment));
            }
            ListQuery::Task {} => return Ok(self.task_page(page)?.map(ListItem::Task)),
            ListQuery::Component {} => (
                NodeKind::Component,
                params![],
                list_sql!("c.status", "JOIN component c ON c.node=n.id", "1"),
            ),
            ListQuery::Path {} => (
                NodeKind::Path,
                params![],
                list_sql!("p.status", "JOIN path p ON p.node=n.id", "1"),
            ),
            ListQuery::State { scope } => (
                NodeKind::State,
                params![scope],
                list_sql!(
                    "s.status",
                    "JOIN state s ON s.node=n.id",
                    "(?3 IS NULL OR s.scope=?3)"
                ),
            ),
            ListQuery::Finding {} => (NodeKind::Finding, params![], list_sql!("NULL", "", "1")),
            ListQuery::Premise { scope, status } => (
                NodeKind::Premise,
                params![scope, status],
                list_sql!(
                    "p.status",
                    "JOIN premise_standing p ON p.node=n.id",
                    "(?3 IS NULL OR p.scope=?3) AND (?4 IS NULL OR p.status=?4)"
                ),
            ),
            ListQuery::Flaw { status, component } => (
                NodeKind::Flaw,
                params![status, component],
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
                },
            ),
            ListQuery::Candidate { root, state } => (
                NodeKind::Candidate,
                params![root, state],
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
                },
            ),
        };
        let bound = [params![page.after, kind], filter].concat();
        self.read(|db| {
            let items = first_rows(db, sql, bound, page.limit(), |row| summary(row, kind))?;
            Ok(Listing::keyset(items, page).map(ListItem::Node))
        })
    }
}

// A keyset page: records after `after`, at most `limit` of them.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Page {
    pub(crate) after: i64,
    pub(crate) limit: Bounded<1, 1000>,
}
impl Page {
    pub(crate) const fn new(after: i64, limit: Bounded<1, 1000>) -> Self {
        Self { after, limit }
    }
    pub(crate) fn limit(self) -> usize {
        u64::from(self.limit) as usize
    }
}
impl Default for Page {
    fn default() -> Self {
        Self::new(0, Bounded::new::<100>())
    }
}

/// One keyset page: the records read and the cursor to continue from, `None` at the end.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct Listing<T> {
    pub(crate) items: Vec<T>,
    pub(crate) next_after: Option<i64>,
}
impl<T> Listing<T> {
    pub(crate) fn map<U>(self, wrap: impl FnMut(T) -> U) -> Listing<U> {
        Listing {
            items: self.items.into_iter().map(wrap).collect(),
            next_after: self.next_after,
        }
    }
}
impl<T> std::ops::Deref for Listing<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.items
    }
}
impl<T> IntoIterator for Listing<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}
impl<T: Keyed> Listing<T> {
    /// A full page continues after its last key; a shorter one is the end.
    pub(crate) fn keyset(items: Vec<T>, page: Page) -> Self {
        let next_after = (items.len() == page.limit())
            .then(|| items.last().map(Keyed::key))
            .flatten();
        Self { items, next_after }
    }
}

/// The record id a keyset page orders by.
pub(crate) trait Keyed {
    fn key(&self) -> i64;
}

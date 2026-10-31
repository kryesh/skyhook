//! The Research page's rows: the first live nodes of one kind, each with what it owes now and
//! its latest task, on it or on a node merged into it.

use std::collections::BTreeMap;

use skyhook::{
    db::{Db, enum_column, optional_enum_column, params},
    job::JobState,
};

use super::super::*;

/// A phase a node owes now with no task of it live, exhausted or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owing {
    pub phase: Phase,
    pub exhausted: bool,
}

/// A node's latest task, by its job's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LastTask {
    pub id: TaskId,
    pub phase: Phase,
    pub state: JobState,
}

#[derive(Clone, Debug)]
pub struct ShelfRow {
    pub node: NodeSummary,
    pub owing: Vec<Owing>,
    pub last: Option<LastTask>,
}

/// The first rows of a kind, and how many live nodes it has.
#[derive(Clone, Debug)]
pub struct Shelf {
    pub total: u64,
    pub rows: Vec<ShelfRow>,
}

/// The first `?1` live nodes of a kind, with their own status.
const fn shelf_sql(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Path => {
            "SELECT n.id,n.title,CASE WHEN p.swept IS NULL THEN 'unswept' ELSE 'swept' END FROM \
             node n JOIN path p ON p.node=n.id ORDER BY p.swept IS NOT NULL,n.id LIMIT ?1"
        }
        NodeKind::Premise => {
            "SELECT n.id,n.title,p.status FROM node n JOIN premise_state p ON p.node=n.id WHERE \
             n.merged_into IS NULL ORDER BY n.id LIMIT ?1"
        }
        NodeKind::Flaw => {
            "SELECT n.id,n.title,f.status FROM node n JOIN flaw_state f ON f.node=n.id WHERE \
             n.merged_into IS NULL ORDER BY n.id LIMIT ?1"
        }
        NodeKind::Candidate => {
            "SELECT n.id,n.title,s.state FROM node n JOIN candidate_state s ON s.candidate=n.id \
             WHERE n.merged_into IS NULL ORDER BY n.id LIMIT ?1"
        }
        NodeKind::Component | NodeKind::Source | NodeKind::Finding => {
            "SELECT n.id,n.title,NULL FROM node n WHERE n.kind=?2 AND n.merged_into IS NULL \
             ORDER BY n.id LIMIT ?1"
        }
    }
}

fn shelf(db: &Db, kind: NodeKind, limit: usize) -> Result<Shelf> {
    let phases: Vec<Phase> = (Phase::ALL.into_iter())
        .filter(|phase| phase.subject() == kind)
        .collect();
    let mut rows = rows(db, shelf_sql(kind), params![limit, kind], |row| {
        let status = match kind {
            NodeKind::Path => optional_enum_column(row, 2)?.map(NodeStatus::Path),
            NodeKind::Premise => optional_enum_column(row, 2)?.map(NodeStatus::Premise),
            NodeKind::Flaw => optional_enum_column(row, 2)?.map(NodeStatus::Flaw),
            NodeKind::Candidate => optional_enum_column(row, 2)?.map(NodeStatus::Candidate),
            NodeKind::Component | NodeKind::Source | NodeKind::Finding => None,
        };
        Ok(ShelfRow {
            node: NodeSummary {
                id: NodeId(row.get(0)?),
                title: row.get(1)?,
                status,
            },
            owing: Vec::new(),
            last: None,
        })
    })?;
    let mut owing = BTreeMap::<NodeId, Vec<Owing>>::new();
    for &phase in phases.iter().filter(|_| !rows.is_empty()) {
        for owed in queues::owed(db, phase, &queues::Spent::default())? {
            owing.entry(owed.node).or_default().push(Owing {
                phase,
                exhausted: owed.exhausted,
            });
        }
    }
    for row in &mut rows {
        row.owing = owing.remove(&row.node.id).unwrap_or_default();
        for &phase in &phases {
            let last = optional(
                db,
                concat!(
                    "SELECT tn.task,s.state FROM task_node tn JOIN task_state s ON \
                     s.task=tn.task WHERE tn.phase=?2 AND ",
                    queues::works!("tn.node", "?1"),
                    " ORDER BY tn.task DESC LIMIT 1"
                ),
                params![row.node.id, phase],
                |found| {
                    Ok(LastTask {
                        id: TaskId(found.get(0)?),
                        phase,
                        state: enum_column(found, 1)?,
                    })
                },
            )?;
            if last.is_some_and(|last| row.last.is_none_or(|held| held.id < last.id)) {
                row.last = last;
            }
        }
    }
    let total = count(
        db,
        "SELECT count(*) FROM node WHERE kind=?1 AND merged_into IS NULL",
        params![kind],
    )?;
    Ok(Shelf { total, rows })
}

impl Store {
    /// The first `limit` live nodes of `kind`, unswept paths first, each with what it owes now
    /// and its latest task.
    pub fn shelf(&self, kind: NodeKind, limit: usize) -> Result<Shelf> {
        self.read(|db| shelf(db, kind, limit))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::super::tests::{begun, fixture, scenario, text},
        *,
    };

    /// A node's latest task includes those begun on a node merged into it.
    #[test]
    fn a_rows_latest_task_follows_merges() {
        let (_session, store, actor) = fixture();
        let (merged, kept) = (
            scenario(&store, &actor, "a").candidate,
            scenario(&store, &actor, "b").candidate,
        );
        let (_, _, task) = begun(&store, &Begin::Assess(vec![merged]));
        let merge = Merge::Candidate {
            candidate: merged,
            into: kept,
            note: text("same"),
        };
        store.merge(&actor, &merge).unwrap();
        let shelf = store.shelf(NodeKind::Candidate, 10).unwrap();
        let rows: Vec<_> = (shelf.rows.iter())
            .map(|row| (row.node.id, row.last.map(|last| last.id)))
            .collect();
        assert_eq!(rows, [(NodeId::from(kept), Some(task))]);
    }
}

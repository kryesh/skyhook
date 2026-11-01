//! The Research page's rows: the first live nodes of one kind, each with what it owes now and
//! its latest task, on it or on a node merged into it.

use std::collections::BTreeMap;

use skyhook::{
    db::{Db, enum_column, params},
    job::JobState,
};

use super::{super::*, list::summary};

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
            "SELECT n.id,n.title,p.status FROM path p JOIN node n ON n.id=p.node WHERE \
             n.merged_into IS NULL ORDER BY p.status!='unswept',p.node LIMIT ?1"
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
        NodeKind::Source => {
            "SELECT n.id,n.title,s.status FROM node n JOIN source s ON s.node=n.id WHERE \
             n.merged_into IS NULL ORDER BY n.id LIMIT ?1"
        }
        NodeKind::Component => {
            "SELECT n.id,n.title,c.status FROM node n JOIN component c ON c.node=n.id WHERE \
             n.merged_into IS NULL ORDER BY n.id LIMIT ?1"
        }
        NodeKind::Finding => {
            "SELECT n.id,n.title,NULL FROM node n WHERE n.kind=?2 AND n.merged_into IS NULL \
             ORDER BY n.id LIMIT ?1"
        }
    }
}

/// The latest task of each of `nodes`, on it or on a node merged into it, with its job's state.
fn last_tasks(db: &Db, nodes: &[NodeId]) -> Result<BTreeMap<NodeId, LastTask>> {
    let found = rows(
        db,
        concat!(
            "SELECT j.value,t.id,t.phase,s.state FROM json_each(?1) j JOIN task t ON t.id=(",
            tasks::newest_tasks!("j.value", "1"),
            ") JOIN job_status s ON s.job=t.job"
        ),
        params![json_list(nodes)?],
        |row| {
            Ok((
                NodeId(row.get(0)?),
                LastTask {
                    id: TaskId(row.get(1)?),
                    phase: enum_column(row, 2)?,
                    state: enum_column(row, 3)?,
                },
            ))
        },
    )?;
    Ok(found.into_iter().collect())
}

fn shelf(db: &Db, kind: NodeKind, limit: usize) -> Result<Shelf> {
    let phases: Vec<Phase> = (Phase::ALL.into_iter())
        .filter(|phase| phase.subject() == kind)
        .collect();
    let mut rows = rows(db, shelf_sql(kind), params![limit, kind], |row| {
        Ok(ShelfRow {
            node: summary(row, kind)?,
            owing: Vec::new(),
            last: None,
        })
    })?;
    let ids: Vec<NodeId> = rows.iter().map(|row| row.node.id).collect();
    let mut owing = BTreeMap::<NodeId, Vec<Owing>>::new();
    for &phase in phases.iter().filter(|_| !ids.is_empty()) {
        for owed in queues::owing(db, phase, &ids)? {
            owing.entry(owed.node).or_default().push(Owing {
                phase,
                exhausted: owed.exhausted,
            });
        }
    }
    let mut last = last_tasks(db, &ids)?;
    for row in &mut rows {
        row.owing = owing.remove(&row.node.id).unwrap_or_default();
        row.last = last.remove(&row.node.id);
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

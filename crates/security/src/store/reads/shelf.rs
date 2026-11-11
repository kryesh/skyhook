//! The Research page's rows: the first live nodes of one kind, each with what it owes now and
//! its latest task, on it or on a node merged into it.

use std::collections::{BTreeMap, BTreeSet};

use skyhook::{
    db::{Db, enum_column, optional_enum_column, params},
    job::JobState,
};

use super::{super::*, list::summary};

/// A phase a node owes now with no task of it live, exhausted or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Owing {
    pub(crate) phase: Phase,
    pub(crate) exhausted: bool,
}

/// A node's latest task, by its job's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LastTask {
    pub(crate) id: TaskId,
    pub(crate) phase: Phase,
    pub(crate) state: JobState,
}

#[derive(Clone, Debug)]
pub(crate) struct ShelfRow {
    pub(crate) node: NodeSummary,
    pub(crate) owing: Vec<Owing>,
    pub(crate) last: Option<LastTask>,
}

/// The first rows of a kind, and how many live nodes it has.
#[derive(Clone, Debug)]
pub(crate) struct Shelf {
    pub(crate) total: u64,
    pub(crate) rows: Vec<ShelfRow>,
}

/// A member of a root group, as the Research page lists it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GroupMember {
    pub(crate) candidate: CandidateId,
    pub(crate) state: Status,
    pub(crate) severity: Option<crate::cvss::Severity>,
    pub(crate) confidence: Option<ConfidenceTier>,
}

/// A candidate's assessment, and whether the results its standing comes from still stand.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Assessed {
    pub(crate) vector: crate::cvss::Cvss40,
    pub(crate) currency: Currency,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Currency {
    Current,
    /// Something it rests on changed since: it is owed again.
    Stale,
}

/// The live nodes of a kind, with their own status; a candidate by its standing as its results
/// last left it.
const fn shelf_sql(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Path => {
            "SELECT n.id,n.title,p.status FROM path p JOIN node n ON n.id=p.node WHERE \
             n.merged_into IS NULL ORDER BY p.status!='unswept',p.node"
        }
        NodeKind::Premise => {
            "SELECT n.id,n.title,p.status FROM node n JOIN premise_standing p ON p.node=n.id WHERE \
             n.merged_into IS NULL ORDER BY n.id"
        }
        NodeKind::Flaw => {
            "SELECT n.id,n.title,f.status FROM node n JOIN flaw_standing f ON f.node=n.id WHERE \
             n.merged_into IS NULL ORDER BY n.id"
        }
        NodeKind::Candidate => {
            "SELECT n.id,n.title,s.standing FROM node n JOIN candidate_state s ON s.candidate=n.id \
             WHERE n.merged_into IS NULL ORDER BY n.id"
        }
        NodeKind::State => {
            "SELECT n.id,n.title,s.status FROM node n JOIN state s ON s.node=n.id WHERE \
             n.merged_into IS NULL ORDER BY n.id"
        }
        NodeKind::Component => {
            "SELECT n.id,n.title,c.status FROM node n JOIN component c ON c.node=n.id WHERE \
             n.merged_into IS NULL ORDER BY n.id"
        }
        NodeKind::Finding => {
            "SELECT n.id,n.title,NULL FROM node n WHERE n.kind=?1 AND n.merged_into IS NULL \
             ORDER BY n.id"
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
    let mut rows = rows(db, shelf_sql(kind), params![kind], |row| {
        Ok(ShelfRow {
            node: summary(row, kind)?,
            owing: Vec::new(),
            last: None,
        })
    })?;
    let ids: Vec<NodeId> = rows.iter().map(|row| row.node.id).collect();
    let mut owing = BTreeMap::<NodeId, Vec<Owing>>::new();
    for phase in Phase::ALL
        .into_iter()
        .filter(|phase| phase.subject() == kind)
    {
        for owed in queues::owed(db, phase, &queues::Spent::default())? {
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
    let facts = Facts::read(db, kind, &ids)?;
    rows.sort_by_cached_key(|row| facts.rank(kind, row));
    let total = rows.len() as u64;
    rows.truncate(limit);
    Ok(Shelf { total, rows })
}

/// Where a row sorts within its section, most relevant first: its standing, then the facts that
/// break ties within it, then its id.
type Rank = [i64; 4];

/// The facts a section sorts its rows by beyond their own status and work.
#[derive(Default)]
struct Facts {
    /// Each scored candidate's score in tenths, and the best score of each flaw's or finding's
    /// candidates.
    scores: BTreeMap<NodeId, i64>,
    demonstrated: BTreeSet<NodeId>,
    /// Paths reaching each component, or accessing each state.
    paths: BTreeMap<NodeId, i64>,
    premise_scopes: BTreeMap<NodeId, PremiseScope>,
    state_scopes: BTreeMap<NodeId, StateScope>,
}

impl Facts {
    fn read(db: &Db, kind: NodeKind, ids: &[NodeId]) -> Result<Self> {
        let mut facts = Self::default();
        let ids = json_list(ids)?;
        let vectors = |sql: &'static str| -> Result<BTreeMap<NodeId, i64>> {
            let mut best = BTreeMap::new();
            for (node, vector) in rows(db, sql, params![ids.clone()], |row| {
                Ok((NodeId(row.get(0)?), row.get::<String>(1)?))
            })? {
                let parsed: crate::cvss::Cvss40 = vector
                    .parse()
                    .map_err(|_| corrupt("invalid stored vector"))?;
                let tenths = (parsed.score() * 10.0).round() as i64;
                let held = best.entry(node).or_insert(tenths);
                *held = (*held).max(tenths);
            }
            Ok(best)
        };
        let counts = |sql: &'static str| -> Result<BTreeMap<NodeId, i64>> {
            let found = rows(db, sql, params![ids.clone()], |row| {
                Ok((NodeId(row.get(0)?), row.get(1)?))
            })?;
            Ok(found.into_iter().collect())
        };
        match kind {
            NodeKind::Candidate => {
                facts.scores = vectors(
                    "SELECT candidate,vector FROM candidate_result WHERE candidate IN (SELECT value \
                     FROM json_each(?1)) AND vector IS NOT NULL",
                )?;
                let sql = "SELECT candidate FROM candidate_core WHERE candidate IN (SELECT value \
                           FROM json_each(?1)) AND demonstrated";
                facts.demonstrated =
                    rows(db, sql, params![ids.clone()], |row| Ok(NodeId(row.get(0)?)))?
                        .into_iter()
                        .collect();
            }
            NodeKind::Flaw => {
                facts.scores = vectors(
                    "SELECT cf.flaw,c.vector FROM candidate_flaw cf JOIN candidate_result c ON \
                     c.candidate=cf.candidate JOIN node cn ON cn.id=c.candidate AND cn.merged_into IS NULL \
                     WHERE cf.flaw IN (SELECT value FROM json_each(?1)) AND c.vector IS NOT NULL",
                )?;
            }
            NodeKind::Finding => {
                facts.scores = vectors(
                    "SELECT f.node,c.vector FROM finding f JOIN candidate_root r ON r.root=f.root \
                     JOIN candidate_result c ON c.candidate=r.candidate JOIN node cn ON \
                     cn.id=c.candidate AND \
                     cn.merged_into IS NULL WHERE f.node IN (SELECT value FROM json_each(?1)) AND \
                     c.vector IS NOT NULL",
                )?;
            }
            NodeKind::Premise => {
                let sql = "SELECT node,scope FROM premise_standing WHERE node IN (SELECT value \
                           FROM json_each(?1))";
                facts.premise_scopes = rows(db, sql, params![ids.clone()], |row| {
                    Ok((NodeId(row.get(0)?), enum_column(row, 1)?))
                })?
                .into_iter()
                .collect();
            }
            NodeKind::Component => {
                facts.paths = counts(
                    "SELECT component,count(DISTINCT path) FROM path_step WHERE component IN \
                     (SELECT value FROM json_each(?1)) GROUP BY component",
                )?;
            }
            NodeKind::State => {
                facts.paths = counts(
                    "SELECT state,count(DISTINCT path) FROM path_state WHERE state IN (SELECT \
                     value FROM json_each(?1)) GROUP BY state",
                )?;
                let sql = "SELECT node,scope FROM state WHERE node IN (SELECT value FROM \
                           json_each(?1))";
                facts.state_scopes = rows(db, sql, params![ids.clone()], |row| {
                    Ok((NodeId(row.get(0)?), enum_column(row, 1)?))
                })?
                .into_iter()
                .collect();
            }
            NodeKind::Path => {}
        }
        Ok(facts)
    }

    /// A row's rank: its status group, then its work (running, owed, exhausted, none), then
    /// what orders it within those.
    fn rank(&self, kind: NodeKind, row: &ShelfRow) -> Rank {
        let id = row.node.id;
        // Higher scores first; none after every score.
        let score = self.scores.get(&id).map_or(i64::MAX, |tenths| -tenths);
        let paths = -self.paths.get(&id).copied().unwrap_or_default();
        let work = work(row) as i64;
        match (kind, row.node.status) {
            (NodeKind::Candidate, Some(NodeStatus::Candidate(status))) => {
                let standing = match status {
                    Status::Validated if self.demonstrated.contains(&id) => 0,
                    Status::Validated => 1,
                    Status::Shortlisted => 2,
                    Status::Unproven => 3,
                    Status::Identified => 4,
                    Status::Noted => 5,
                    Status::BelowThreshold => 6,
                    Status::Excluded => 7,
                    Status::Disproved => 8,
                    Status::Merged => 9,
                };
                [standing, work, score, -id.0]
            }
            (NodeKind::Finding, _) => [score, id.0, 0, 0],
            (NodeKind::Path, Some(NodeStatus::Path(status))) => {
                let standing = match status {
                    PathStatus::Unswept => 0,
                    PathStatus::Swept => 1,
                    PathStatus::Retracted => 2,
                };
                [standing, work, id.0, 0]
            }
            (NodeKind::Flaw, Some(NodeStatus::Flaw(status))) => {
                let standing = match status {
                    FlawStatus::Confirmed => 0,
                    FlawStatus::Suspected => 1,
                    FlawStatus::Refuted => 2,
                };
                [standing, work, score, id.0]
            }
            (NodeKind::Premise, Some(NodeStatus::Premise(status))) => {
                let scope = self.premise_scopes.get(&id).copied();
                let standing = match (status, scope) {
                    (PremiseStatus::Undecided, Some(PremiseScope::Deployment)) => 0,
                    (PremiseStatus::Undecided, _) => 1,
                    // A decided premise owing its settle again is disputed.
                    (_, _) if work != RowWork::None as i64 => 2,
                    (PremiseStatus::Holds, _) => 3,
                    (PremiseStatus::Fails, _) => 4,
                };
                let scope = i64::from(scope == Some(PremiseScope::Deployment));
                [standing, work, scope, id.0]
            }
            (NodeKind::Component, status) => {
                let retracted = status == Some(NodeStatus::Inventory(InventoryStatus::Retracted));
                [i64::from(retracted), paths, id.0, 0]
            }
            (NodeKind::State, status) => {
                let scope = self.state_scopes.get(&id).copied();
                let checkable = matches!(scope, Some(StateScope::Instance | StateScope::Handed));
                let standing = match status {
                    Some(NodeStatus::Inventory(InventoryStatus::Retracted)) => 2,
                    _ if checkable => 0,
                    _ => 1,
                };
                [standing, work, paths, id.0]
            }
            _ => [i64::MAX, id.0, 0, 0],
        }
    }
}

/// What work a row's node has: a live task, work owed, work owed but exhausted, or none.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowWork {
    Running = 0,
    Owed = 1,
    Exhausted = 2,
    None = 3,
}

fn work(row: &ShelfRow) -> RowWork {
    if row.last.is_some_and(|task| !task.state.is_terminal()) {
        RowWork::Running
    } else if row.owing.iter().any(|owing| !owing.exhausted) {
        RowWork::Owed
    } else if row.owing.is_empty() {
        RowWork::None
    } else {
        RowWork::Exhausted
    }
}

impl Store {
    /// The first `limit` live nodes of `kind`, unswept paths first, each with what it owes now
    /// and its latest task.
    pub(crate) fn shelf(&self, kind: NodeKind, limit: usize) -> Result<Shelf> {
        self.read(|db| shelf(db, kind, limit))
    }

    /// Which other candidate each of `candidates` waits behind, and why.
    pub(crate) fn behind(&self, candidates: &[NodeId]) -> Result<BTreeMap<NodeId, Behind>> {
        self.read(|db| Ok(queues::behind(db, candidates)?.into_iter().collect()))
    }

    /// The live members of `candidate`'s root group, most severe first, each by its state.
    pub(crate) fn root_group(&self, candidate: NodeId) -> Result<Vec<GroupMember>> {
        self.read(|db| {
            let sql = "SELECT s.candidate,s.state,s.severity,s.confidence FROM candidate_root r \
                       JOIN candidate_root o ON o.root=r.root JOIN candidate_state s ON \
                       s.candidate=o.candidate WHERE r.candidate=?1 AND s.merged_into IS NULL ORDER \
                       BY s.rank IS NULL,s.rank DESC,s.candidate";
            rows(db, sql, params![candidate], |row| {
                Ok(GroupMember {
                    candidate: CandidateId(row.get(0)?),
                    state: enum_column(row, 1)?,
                    severity: optional_enum_column(row, 2)?,
                    confidence: optional_enum_column(row, 3)?,
                })
            })
        })
    }

    /// The assessments of those of `candidates` that were assessed, each with whether its
    /// results still stand.
    pub(crate) fn assessments(&self, candidates: &[NodeId]) -> Result<BTreeMap<NodeId, Assessed>> {
        self.read(|db| {
            let sql = "SELECT candidate,vector,stale FROM candidate_state WHERE candidate IN \
                       (SELECT value FROM json_each(?1)) AND vector IS NOT NULL";
            let found = rows(db, sql, params![json_list(candidates)?], |row| {
                let vector: String = row.get(1)?;
                let vector = vector
                    .parse()
                    .map_err(|_| corrupt("invalid stored vector"))?;
                let currency = match row.get::<bool>(2)? {
                    true => Currency::Stale,
                    false => Currency::Current,
                };
                Ok((NodeId(row.get(0)?), Assessed { vector, currency }))
            })?;
            Ok(found.into_iter().collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::{
        HIGH, assess, begun, candidate, decide, fixture, flaw, judge, premise, scenario, test,
        text, verdict,
    };

    /// Each section lists its most relevant rows first: candidates by standing then score, a
    /// confirmed flaw before suspected ones, and an undecided deployment premise, the user's to
    /// answer, before an owed code premise and a decided one.
    #[test]
    fn sections_list_their_most_relevant_rows_first() {
        let (_session, store, actor) = fixture();
        let [unassessed, minor, severe] =
            ["a", "b", "c"].map(|name| scenario(&store, &actor, name));
        let medium = "CVSS:4.0/AV:N/AC:L/AT:N/PR:L/UI:N/VC:L/VI:L/VA:N/SC:N/SI:N/SA:N";
        assess(&store, minor.candidate, medium);
        assess(&store, severe.candidate, HIGH);
        let order = |kind| -> Vec<NodeId> {
            (store.shelf(kind, 10).unwrap().rows.iter())
                .map(|row| row.node.id)
                .collect()
        };
        assert_eq!(
            order(NodeKind::Candidate),
            [severe.candidate, unassessed.candidate, minor.candidate].map(NodeId::from)
        );

        judge(&store, minor.flaw, FlawStatus::Confirmed);
        assert_eq!(order(NodeKind::Flaw)[0], NodeId::from(minor.flaw));

        let [decided, code, deployment] = [
            PremiseScope::Code,
            PremiseScope::Code,
            PremiseScope::Deployment,
        ]
        .map(|scope| premise(&store, &actor, scope));
        let links = [decided, code, deployment].map(|premise| Edge::FlawPremise {
            flaw: severe.flaw,
            premise,
        });
        store.relink(&actor, &links, &[]).unwrap();
        decide(&store, decided, PremiseStatus::Holds);
        assert_eq!(
            order(NodeKind::Premise),
            [deployment, code, decided].map(NodeId::from)
        );
    }

    /// Within a status group, rows owing work come first, then those whose work is exhausted,
    /// then those owing none.
    #[test]
    fn rows_owing_work_lead_their_group_exhausted_before_none() {
        let (_session, store, actor) = fixture();
        let decoder = scenario(&store, &actor, "decoder");
        let [exhausted, owed, idle] = [(); 3].map(|_| premise(&store, &actor, PremiseScope::Code));
        let links = [exhausted, owed].map(|premise| Edge::FlawPremise {
            flaw: decoder.flaw,
            premise,
        });
        store.relink(&actor, &links, &[]).unwrap();
        crate::store::work::queues::tests::exhaust(&store, &Begin::Settle(Listed::of([exhausted])));
        let order: Vec<NodeId> = (store.shelf(NodeKind::Premise, 10).unwrap().rows.iter())
            .map(|row| row.node.id)
            .collect();
        assert_eq!(order, [owed, exhausted, idle].map(NodeId::from));
    }

    /// A validated candidate waits behind its root group's first contender for a test while it
    /// contends for one, and once that one is demonstrated at its severity it is noted behind it.
    #[test]
    fn a_candidate_waits_behind_its_groups_lead() {
        let (_session, store, actor) = fixture();
        let lead = scenario(&store, &actor, "decoder");
        let other = flaw(&store, &actor, lead.component);
        let variant = candidate(&store, &actor, lead.path, &[lead.flaw, other]);
        for id in [lead.candidate, variant] {
            assess(&store, id, HIGH);
            verdict(&store, id, Verdict::Validated);
        }
        let behind = |id: CandidateId| store.behind(&[id.into()]).unwrap().remove(&id.into());
        let first = lead.candidate;
        assert_eq!(behind(variant), Some(Behind::Test { lead: first }));
        assert_eq!(behind(first), None);
        test(&store, &actor, variant, TestResult::Declined);
        assert_eq!(behind(variant), None, "a declined test owes nothing");
        test(&store, &actor, first, TestResult::Confirmed);
        assert_eq!(behind(variant), Some(Behind::Noted { lead: first }));
    }

    /// A root group shows a member demonstrated only while it stands so: a confirmed test of an
    /// unproven candidate observed a premise and demonstrates nothing.
    #[test]
    fn a_group_member_is_demonstrated_only_while_it_stands_so() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let id = scenario.candidate;
        let code = premise(&store, &actor, PremiseScope::Code);
        let rests = Edge::FlawPremise {
            flaw: scenario.flaw,
            premise: code,
        };
        store.relink(&actor, &[rests], &[]).unwrap();
        assess(&store, id, HIGH);
        verdict(&store, id, Verdict::Unproven);
        test(&store, &actor, id, TestResult::Confirmed);
        let [member] = store.root_group(id.into()).unwrap()[..] else {
            panic!("one member")
        };
        assert_eq!(
            (member.state, member.confidence),
            (Status::Unproven, Some(ConfidenceTier::Conjectured))
        );
    }

    /// A node's latest task includes those begun on a node merged into it.
    #[test]
    fn a_rows_latest_task_follows_merges() {
        let (_session, store, actor) = fixture();
        let (merged, kept) = (
            scenario(&store, &actor, "a").candidate,
            scenario(&store, &actor, "b").candidate,
        );
        let (_, _, task) = begun(&store, &Begin::Review(Listed::of([merged])));
        let merge = Merge {
            node: merged.into(),
            into: kept.into(),
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

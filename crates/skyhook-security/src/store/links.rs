//! Edges linked and unlinked, and merges of duplicate nodes, which move every edge of the merged
//! node to the node it is merged into. A path's steps keep
//! positions 0..n-1 through every write; a candidate's chain and actions are set by its upsert
//! or a merge, rewritten past the prefix they keep, or one flaw replaced in place.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::Deserialize;
use skyhook::db::{Db, params};

use super::*;

/// An edge to add. An ordered edge goes at `position`, or last.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Link {
    PathStep {
        path: PathId,
        component: ComponentId,
        location: Option<Location>,
        note: Text,
        position: Option<u32>,
    },
    /// Where the path's flow reads or writes the state.
    PathState {
        path: PathId,
        state: StateId,
        access: Access,
        location: Location,
    },
    PathPremise {
        path: PathId,
        premise: PremiseId,
    },
    FlawPremise {
        flaw: FlawId,
        premise: PremiseId,
    },
    /// The premise holds only if `rests_on` does.
    PremiseBasis {
        premise: PremiseId,
        rests_on: PremiseId,
    },
    /// The flaw leaves the state wrong.
    FlawState {
        flaw: FlawId,
        state: StateId,
    },
}

/// An edge to remove; a path's step by its position.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Unlink {
    PathStep {
        path: PathId,
        position: u32,
    },
    PathState {
        path: PathId,
        state: StateId,
        access: Access,
        location: Location,
    },
    PathPremise {
        path: PathId,
        premise: PremiseId,
    },
    FlawPremise {
        flaw: FlawId,
        premise: PremiseId,
    },
    PremiseBasis {
        premise: PremiseId,
        rests_on: PremiseId,
    },
    FlawState {
        flaw: FlawId,
        state: StateId,
    },
}

/// The statements keeping path `?1`'s step positions contiguous: how many it has, read off
/// its last position, and the two-step shifts opening a gap at `?2` or closing the one at `?2`.
/// Each shift first lifts the moving rows past every position in use, so no step collides with
/// a row not yet moved.
struct Ordered {
    count: &'static str,
    open: [&'static str; 2],
    close: [&'static str; 2],
}

const PATH_STEPS: Ordered = Ordered {
    count: "SELECT coalesce(max(position)+1,0) FROM path_step WHERE path=?1",
    open: [
        "UPDATE path_step SET position=position+4294967296 WHERE path=?1 AND position>=?2",
        "UPDATE path_step SET position=position-4294967295 WHERE path=?1 AND position>=4294967296",
    ],
    close: [
        "UPDATE path_step SET position=position+4294967296 WHERE path=?1 AND position>?2",
        "UPDATE path_step SET position=position-4294967297 WHERE path=?1 AND position>=4294967296",
    ],
};

impl Ordered {
    /// Open a gap for a new edge of `owner` at `position`, or last: where it goes.
    fn open(&self, db: &Db, owner: impl Into<NodeId>, position: Option<u32>) -> Result<i64> {
        let owner = owner.into();
        let count = count(db, self.count, params![owner])? as i64;
        let position = position.map_or(count, |position| count.min(position.into()));
        for sql in self.open {
            db.execute(sql, params![owner, position])?;
        }
        Ok(position)
    }

    /// Close the gap a removed edge of `owner` left at `position`.
    fn close(&self, db: &Db, owner: impl Into<NodeId>, position: i64) -> Result<()> {
        let owner = owner.into();
        for sql in self.close {
            db.execute(sql, params![owner, position])?;
        }
        Ok(())
    }
}

/// How an edit bears on its two ends: the first end changes, or its standing moves, while the
/// edit only concerns the second.
fn ends(
    marks: &mut Marks,
    edited: bool,
    first: impl Into<NodeId>,
    effect: EventEffect,
    second: impl Into<NodeId>,
) {
    let effect = if edited {
        effect
    } else {
        EventEffect::Concerns
    };
    marks.mark(first, effect);
    marks.mark(second, EventEffect::Concerns);
}

fn link(db: &Db, link: &Link, marks: &mut Marks) -> Result<()> {
    match link {
        Link::PathStep {
            path,
            component,
            location,
            note,
            position,
        } => {
            let (path, component) = (live(db, *path)?, live(db, *component)?);
            let location = location
                .as_ref()
                .map(|at| location_id(db, at))
                .transpose()?;
            let restated = exists(
                db,
                "SELECT 1 FROM path_step WHERE path=?1 AND component=?2 AND location IS ?3 AND \
                 note=?4",
                params![path, component, location, note],
            )?;
            if !restated {
                let position = PATH_STEPS.open(db, path, *position)?;
                db.execute(
                    "INSERT INTO path_step(path,position,component,location,note) VALUES \
                     (?1,?2,?3,?4,?5)",
                    params![path, position, component, location, note],
                )?;
            }
            ends(marks, !restated, path, EventEffect::Changed, component);
        }
        Link::PathState {
            path,
            state,
            access,
            location,
        } => {
            let (path, state) = (live(db, *path)?, live(db, *state)?);
            let sql = "INSERT INTO path_state(path,state,access,location) VALUES (?1,?2,?3,?4) ON \
                       CONFLICT DO NOTHING";
            let at = location_id(db, location)?;
            let added = db.execute(sql, params![path, state, *access, at])? > 0;
            accessed(marks, added, path, state);
        }
        Link::PathPremise { path, premise } => {
            let (path, premise) = (live(db, *path)?, live(db, *premise)?);
            let sql =
                "INSERT INTO path_premise(path,premise) VALUES (?1,?2) ON CONFLICT DO NOTHING";
            let added = db.execute(sql, params![path, premise])? > 0;
            ends(marks, added, path, EventEffect::Standing, premise);
        }
        Link::FlawPremise { flaw, premise } => {
            let (flaw, premise) = (live(db, *flaw)?, live(db, *premise)?);
            let sql =
                "INSERT INTO flaw_premise(flaw,premise) VALUES (?1,?2) ON CONFLICT DO NOTHING";
            let added = db.execute(sql, params![flaw, premise])? > 0;
            ends(marks, added, flaw, EventEffect::Standing, premise);
        }
        Link::PremiseBasis { premise, rests_on } => {
            let (premise, rests_on) = basis_ends(db, *premise, *rests_on)?;
            let sql = "INSERT INTO premise_basis(premise,rests_on) VALUES (?1,?2) ON CONFLICT DO \
                       NOTHING";
            db.execute(sql, params![premise, rests_on])?;
            ends(marks, false, premise, EventEffect::Concerns, rests_on);
        }
        Link::FlawState { flaw, state } => {
            let (flaw, state) = (live(db, *flaw)?, live(db, *state)?);
            let sql = "INSERT INTO flaw_state(flaw,state) VALUES (?1,?2) ON CONFLICT DO NOTHING";
            let added = db.execute(sql, params![flaw, state])? > 0;
            ends(marks, added, flaw, EventEffect::Standing, state);
        }
    }
    Ok(())
}

/// The live ends of a premise's basis, two premises.
fn basis_ends(db: &Db, premise: PremiseId, rests_on: PremiseId) -> Result<(PremiseId, PremiseId)> {
    let (premise, rests_on) = (live(db, premise)?, live(db, rests_on)?);
    if premise == rests_on {
        return Err(Error::SelfBasis);
    }
    Ok((premise, rests_on))
}

/// How an access recorded or removed bears on its path and state: it moves their own work
/// alone, and the work reading the state's accesses; restated, it only concerns them.
fn accessed(marks: &mut Marks, edited: bool, path: PathId, state: StateId) {
    match edited {
        true => {
            marks.alone(path);
            marks.alone(state);
        }
        false => {
            marks.mark(path, EventEffect::Concerns);
            marks.mark(state, EventEffect::Concerns);
        }
    }
}

fn unlink(db: &Db, unlink: &Unlink, marks: &mut Marks) -> Result<()> {
    match unlink {
        Unlink::PathStep { path, position } => {
            let path = live(db, *path)?;
            let component = optional(
                db,
                "DELETE FROM path_step WHERE path=?1 AND position=?2 RETURNING component",
                params![path, *position],
                |row| Ok(ComponentId(row.get(0)?)),
            )?;
            match component {
                Some(component) => {
                    PATH_STEPS.close(db, path, (*position).into())?;
                    ends(marks, true, path, EventEffect::Changed, component);
                }
                None => marks.mark(path, EventEffect::Concerns),
            }
        }
        Unlink::PathState {
            path,
            state,
            access,
            location,
        } => {
            let (path, state) = (live(db, *path)?, live(db, *state)?);
            let sql = "DELETE FROM path_state WHERE path=?1 AND state=?2 AND access=?3 AND \
                       location=?4";
            let at = location_id(db, location)?;
            let removed = db.execute(sql, params![path, state, *access, at])? > 0;
            accessed(marks, removed, path, state);
        }
        Unlink::PathPremise { path, premise } => {
            let (path, premise) = (live(db, *path)?, live(db, *premise)?);
            let sql = "DELETE FROM path_premise WHERE path=?1 AND premise=?2";
            let removed = db.execute(sql, params![path, premise])? > 0;
            ends(marks, removed, path, EventEffect::Standing, premise);
        }
        Unlink::FlawPremise { flaw, premise } => {
            let (flaw, premise) = (live(db, *flaw)?, live(db, *premise)?);
            let sql = "DELETE FROM flaw_premise WHERE flaw=?1 AND premise=?2";
            let removed = db.execute(sql, params![flaw, premise])? > 0;
            ends(marks, removed, flaw, EventEffect::Standing, premise);
        }
        Unlink::PremiseBasis { premise, rests_on } => {
            let (premise, rests_on) = basis_ends(db, *premise, *rests_on)?;
            let sql = "DELETE FROM premise_basis WHERE premise=?1 AND rests_on=?2";
            db.execute(sql, params![premise, rests_on])?;
            ends(marks, false, premise, EventEffect::Concerns, rests_on);
        }
        Unlink::FlawState { flaw, state } => {
            let (flaw, state) = (live(db, *flaw)?, live(db, *state)?);
            let sql = "DELETE FROM flaw_state WHERE flaw=?1 AND state=?2";
            let removed = db.execute(sql, params![flaw, state])? > 0;
            ends(marks, removed, flaw, EventEffect::Standing, state);
        }
    }
    Ok(())
}

/// Fold duplicate `node`, of any kind but a finding, into `into`, a node of its kind: its edges
/// move to `into`.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Merge {
    pub node: NodeId,
    pub into: NodeId,
    pub note: Text,
}

/// The statements moving one kind of edge from node `?1` to node `?2`: the nodes at their
/// other ends, the move, and, where `?2` already had such an edge, the duplicates left behind.
struct Repoint {
    others: &'static str,
    moved: &'static str,
    duplicates: Option<&'static str>,
}

macro_rules! repoint {
    ($table:literal, $column:literal, $other:literal) => {
        repoint!(@ $table, $column, $other, None)
    };
    ($table:literal, $column:literal, $other:literal, duplicates) => {
        repoint!(@ $table, $column, $other,
            Some(concat!("DELETE FROM ", $table, " WHERE ", $column, "=?1")))
    };
    (@ $table:literal, $column:literal, $other:literal, $duplicates:expr) => {
        Repoint {
            others: concat!("SELECT ", $other, " FROM ", $table, " WHERE ", $column, "=?1"),
            moved: concat!("UPDATE OR IGNORE ", $table, " SET ", $column, "=?2 WHERE ", $column, "=?1"),
            duplicates: $duplicates,
        }
    };
}

/// Move every edge of `repoints` from `from` to `into`, marking the nodes at their other ends.
fn repoint(
    db: &Db,
    repoints: &[Repoint],
    from: NodeId,
    into: NodeId,
    marks: &mut Marks,
) -> Result<()> {
    for repoint in repoints {
        for other in rows(db, repoint.others, params![from], |row| {
            Ok(row.get::<Option<i64>>(0)?)
        })?
        .into_iter()
        .flatten()
        {
            marks.mark(NodeId(other), EventEffect::Standing);
        }
        db.execute(repoint.moved, params![from, into])?;
        if let Some(duplicates) = repoint.duplicates {
            db.execute(duplicates, params![from])?;
        }
    }
    Ok(())
}

/// `from`'s evidence moves to `into`, and `from` and what was merged into it point at `into`,
/// retargeted by merge `event`: the merge changes both, and only concerns each node it repoints.
fn absorb(db: &Db, event: EventId, from: NodeId, into: NodeId, marks: &mut Marks) -> Result<()> {
    db.execute(
        "UPDATE evidence SET node=?2 WHERE node=?1",
        params![from, into],
    )?;
    db.execute(
        "INSERT INTO node_merge(node,event,kind,survivor) SELECT id,?1,kind,?3 FROM node WHERE \
         id=?2 OR merged_into=?2",
        params![event, from, into],
    )?;
    let repointed = rows(
        db,
        "UPDATE node SET merged_into=?2 WHERE merged_into=?1 RETURNING id",
        params![from, into],
        |row| Ok(NodeId(row.get(0)?)),
    )?;
    db.execute(
        "UPDATE node SET merged_into=?2 WHERE id=?1",
        params![from, into],
    )?;
    for node in repointed {
        marks.mark(node, EventEffect::Concerns);
    }
    marks.mark(from, EventEffect::Changed);
    marks.mark(into, EventEffect::Changed);
    Ok(())
}

/// Flaw `from`'s places in chains go to `into`: a chain holding both keeps the earlier place,
/// so its root stays the flaw it was or the one that flaw became. The root's finding follows
/// it, or is absorbed by `into`'s.
fn rechain(db: &Db, event: EventId, from: FlawId, into: FlawId, marks: &mut Marks) -> Result<()> {
    let held = rows(
        db,
        "SELECT candidate,EXISTS (SELECT 1 FROM candidate_flaw WHERE candidate=f.candidate AND \
         flaw=?2) FROM candidate_flaw f WHERE flaw=?1",
        params![from, into],
        |row| Ok((NodeId(row.get(0)?), row.get::<bool>(1)?)),
    )?;
    for (candidate, collides) in held {
        marks.mark(candidate, EventEffect::Standing);
        if !collides {
            db.execute(
                "UPDATE candidate_flaw SET flaw=?3 WHERE candidate=?1 AND flaw=?2",
                params![candidate, from, into],
            )?;
            marks.mark(from, EventEffect::Concerns);
            marks.mark(into, EventEffect::Concerns);
            continue;
        }
        let mut seen = BTreeSet::new();
        let chain: Vec<FlawId> = (FLAWS.held::<FlawId>(db, candidate)?.into_iter())
            .map(|flaw| if flaw == from { into } else { flaw })
            .filter(|flaw| seen.insert(NodeId::from(*flaw)))
            .collect();
        FLAWS.set(db, candidate, &chain, marks)?;
    }
    let finding = |root: FlawId| {
        optional(
            db,
            "SELECT node FROM finding WHERE root=?1",
            params![root],
            |row| Ok(NodeId(row.get(0)?)),
        )
    };
    match (finding(from)?, finding(into)?) {
        (Some(absorbed), Some(kept)) => {
            absorb(db, event, absorbed, kept, marks)?;
        }
        (Some(rekeyed), None) => {
            db.execute(
                "UPDATE finding SET root=?2 WHERE node=?1",
                params![rekeyed, into],
            )?;
            marks.mark(rekeyed, EventEffect::Standing);
        }
        (None, _) => {}
    }
    Ok(())
}

/// Fold live duplicate `from` into survivor `into`, both of `kind`: every edge naming it names
/// the survivor, marking the nodes at their other ends. A finding is not merged. A merged input
/// state gives up its trust and location, so its location is free for a live one, and a merged
/// state its check with the premises it rests on. A merged path keeps its steps as history, the survivor's standing for the
/// flow, and its states' accesses move to the survivor, moving their checks. A merged candidate
/// keeps its chain, actions, tests and verdict as history, and leaves its root's group as a
/// candidate losing its root does; the survivor performs, after its own actions, each of the
/// duplicate's it has no counterpart for: as many of a path as the duplicate performs beyond its
/// own.
fn fold(
    db: &Db,
    event: EventId,
    kind: NodeKind,
    from: NodeId,
    into: NodeId,
    marks: &mut Marks,
) -> Result<()> {
    match kind {
        NodeKind::Component => {
            let edges = [
                repoint!("component_file", "component", "NULL"),
                repoint!("component_interface", "component", "NULL"),
                repoint!("path_step", "component", "path"),
                repoint!("flaw", "component", "node"),
            ];
            repoint(db, &edges, from, into, marks)
        }
        NodeKind::State => {
            let edges = [
                repoint!("path_state", "state", "path", duplicates),
                repoint!("flaw_state", "state", "flaw", duplicates),
            ];
            repoint(db, &edges, from, into, marks)?;
            for sql in [
                "DELETE FROM input_state WHERE state=?1",
                "DELETE FROM state_premise WHERE state=?1",
                "DELETE FROM state_check WHERE state=?1",
            ] {
                db.execute(sql, params![from])?;
            }
            Ok(())
        }
        NodeKind::Path => {
            marks.accesses(db, PathId::of(from))?;
            let edges = [
                repoint!("path_state", "path", "NULL", duplicates),
                repoint!("path_premise", "path", "premise", duplicates),
                repoint!("candidate_action", "path", "candidate"),
            ];
            repoint(db, &edges, from, into, marks)
        }
        NodeKind::Premise => {
            let edges = [
                repoint!("flaw_premise", "premise", "flaw", duplicates),
                repoint!("path_premise", "premise", "path", duplicates),
                repoint!("state_premise", "premise", "NULL", duplicates),
                repoint!("premise_basis", "premise", "rests_on", duplicates),
                repoint!("premise_basis", "rests_on", "premise", duplicates),
            ];
            repoint(db, &edges, from, into, marks)
        }
        NodeKind::Flaw => {
            let edges = [
                repoint!("flaw_premise", "flaw", "premise", duplicates),
                repoint!("flaw_state", "flaw", "NULL", duplicates),
            ];
            repoint(db, &edges, from, into, marks)?;
            rechain(db, event, FlawId::of(from), FlawId::of(into), marks)
        }
        NodeKind::Candidate => {
            let edges = [repoint!("environment", "candidate", "NULL")];
            repoint(db, &edges, from, into, marks)?;
            let mut actions: Vec<PathId> = ACTIONS.held(db, into)?;
            let mut unmatched = BTreeMap::<NodeId, usize>::new();
            for path in &actions {
                *unmatched.entry((*path).into()).or_default() += 1;
            }
            for path in ACTIONS.held::<PathId>(db, from)? {
                match unmatched.get_mut(&path.into()) {
                    Some(count) if *count > 0 => *count -= 1,
                    _ => actions.push(path),
                }
            }
            ACTIONS.set(db, into, &actions, marks)?;
            let root = optional(
                db,
                "SELECT root FROM candidate_root WHERE candidate=?1",
                params![from],
                |row| Ok(FlawId(row.get(0)?)),
            )?;
            marks.rerooted(root, None);
            Ok(())
        }
        NodeKind::Finding => Err(Error::FindingMerge),
    }
}

impl Store {
    /// Add edges, as one event; an edge already there is restated.
    pub fn link(&self, actor: &AgentId, links: &[Link]) -> Result<()> {
        if links.len() > MAX_ATTACHMENTS {
            return Err(Error::Limit(Bound::Attachments));
        }
        self.write(|db| {
            audit(db, actor, EventKind::Linked, None, |_| {
                let mut marks = Marks::default();
                for edge in links {
                    link(db, edge, &mut marks)?;
                }
                Ok(((), marks))
            })
        })
    }

    /// Remove edges, as one event; an edge not there is restated absent.
    pub fn unlink(&self, actor: &AgentId, unlinks: &[Unlink]) -> Result<()> {
        if unlinks.len() > MAX_ATTACHMENTS {
            return Err(Error::Limit(Bound::Attachments));
        }
        self.write(|db| {
            audit(db, actor, EventKind::Unlinked, None, |_| {
                let mut marks = Marks::default();
                for edge in unlinks {
                    unlink(db, edge, &mut marks)?;
                }
                Ok(((), marks))
            })
        })
    }

    /// Merge a duplicate node into a live node of its kind, with the actor's reason; ids of
    /// merged nodes name the node they were merged into, so two already one are left as they are.
    pub fn merge(&self, actor: &AgentId, merge: &Merge) -> Result<()> {
        self.write(|db| {
            let (from, kind) = live_node(db, merge.node)?;
            let (into, into_kind) = live_node(db, merge.into)?;
            if kind != into_kind {
                return Err(Error::MergeKinds {
                    node: merge.node,
                    node_kind: kind,
                    into: merge.into,
                    into_kind,
                });
            }
            if merge.node == merge.into {
                return Err(Error::SelfMerge);
            }
            if from == into {
                return Ok(());
            }
            audit(db, actor, EventKind::Merged, Some(&merge.note), |event| {
                let mut marks = Marks::default();
                fold(db, event, kind, from, into, &mut marks)?;
                absorb(db, event, from, into, &mut marks)?;
                Ok(((), marks))
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::{
        super::{
            queues::tests::queued,
            tests::{
                HIGH, assess, begun, candidate, component, daemon, decide, end_job, fixture, flaw,
                last_event, marks, path, path_from, premise, scenario, text, verdict,
            },
        },
        *,
    };

    fn steps(store: &Store, path: PathId) -> Vec<(u32, ComponentId)> {
        let NodeView::Path(view) = store.node(path.into()).unwrap() else {
            panic!("a path")
        };
        (view.steps.iter())
            .map(|step| (step.position, ComponentId(step.component.id.0)))
            .collect()
    }

    fn chain(store: &Store, candidate: CandidateId) -> Vec<FlawId> {
        let NodeView::Candidate(view) = store.node(candidate.into()).unwrap() else {
            panic!("a candidate")
        };
        view.chain.iter().map(|link| link.flaw.id).collect()
    }

    fn actions(store: &Store, candidate: CandidateId) -> Vec<NodeId> {
        let NodeView::Candidate(view) = store.node(candidate.into()).unwrap() else {
            panic!("a candidate")
        };
        view.actions.iter().map(|action| action.path.id).collect()
    }

    /// Steps go where they are put, or last, and removing one closes its gap: positions stay
    /// 0..n-1. A chain is set whole, each flaw once, and a candidate's root is its first flaw.
    #[test]
    fn ordered_edges_keep_contiguous_positions() {
        let (_session, store, actor) = fixture();
        let [a, b, c] = ["a", "b", "c"].map(|name| component(&store, &actor, name));
        let route = path(&store, &actor, "route", &[a, b]);
        let step = |component, note: &str, position| Link::PathStep {
            path: route,
            component,
            location: None,
            note: text(note),
            position,
        };
        store.link(&actor, &[step(c, "enters", Some(0))]).unwrap();
        store
            .link(
                &actor,
                &[step(a, "returns", None), step(b, "again", Some(99))],
            )
            .unwrap();
        store.link(&actor, &[step(c, "enters", Some(3))]).unwrap();
        assert_eq!(
            marks(&store, last_event(&store)),
            [
                (c.into(), EventEffect::Concerns),
                (route.into(), EventEffect::Concerns)
            ],
            "an identical step is restated"
        );
        let unlink = |position| Unlink::PathStep {
            path: route,
            position,
        };
        store.unlink(&actor, &[unlink(1), unlink(9)]).unwrap();
        assert_eq!(steps(&store, route), [(0, c), (1, b), (2, a), (3, b)]);

        let [first, second, third] = [(); 3].map(|_| flaw(&store, &actor, a));
        let id = candidate(&store, &actor, route, &[third, first, second, first]);
        assert_eq!(chain(&store, id), [third, first, second]);
        let rooted = |root| {
            let query = ListQuery::Candidate {
                root: Some(root),
                state: None,
            };
            let listed = store.list(&query, Page::default()).unwrap();
            listed.iter().map(|summary| summary.id).collect::<Vec<_>>()
        };
        assert_eq!(rooted(third), [NodeId::from(id)]);
        let edit = Edit {
            title: None,
            body: None,
            detail: CandidateEdit {
                actions: None,
                chain: Some(vec![first].try_into().unwrap()),
            },
        };
        let change = Upsert::Candidate(Change::Edit(id, edit));
        store.upsert(&actor, &change).unwrap();
        assert_eq!(chain(&store, id), [first]);
        assert_eq!(rooted(first), [NodeId::from(id)]);
        assert!(rooted(third).is_empty());
    }

    /// A merge moves every edge of the merged node to the survivor, dropping duplicates; a
    /// chain holding both flaws keeps the earlier place, so its root stays the flaw that place
    /// became, with the root's finding re-keyed or absorbed; a merged candidate's actions
    /// follow the survivor's, each path as often as the merged one performs it beyond the
    /// survivor; and `merged_into` stays one hop: writes naming a merged node land on the
    /// survivor.
    #[test]
    fn a_merge_moves_every_edge_to_the_survivor() {
        let (_session, store, actor) = fixture();
        let (one, two) = (
            scenario(&store, &actor, "one"),
            scenario(&store, &actor, "two"),
        );
        let [first_premise, second_premise, resting] =
            [(); 3].map(|_| premise(&store, &actor, PremiseScope::Code));
        let rests = |premise, rests_on| Link::PremiseBasis { premise, rests_on };
        assert_matches!(
            store.link(&actor, &[rests(resting, resting)]),
            Err(Error::SelfBasis)
        );
        let links = [
            rests(first_premise, second_premise),
            rests(resting, first_premise),
            Link::FlawPremise {
                flaw: one.flaw,
                premise: first_premise,
            },
            Link::FlawPremise {
                flaw: two.flaw,
                premise: second_premise,
            },
            Link::PathPremise {
                path: one.path,
                premise: first_premise,
            },
            Link::PathPremise {
                path: one.path,
                premise: second_premise,
            },
        ];
        store.link(&actor, &links).unwrap();
        let between = flaw(&store, &actor, two.component);
        let both = candidate(&store, &actor, two.path, &[one.flaw, between, two.flaw]);
        let rekeyed = store
            .report(&actor, one.flaw, &text("finding"), &text("report"))
            .unwrap();
        let environment = NewEnvironment {
            instructions: text("start it"),
            candidate: Some(one.candidate),
            serves: Serves::default(),
        };
        store.environment(&actor, &environment).unwrap();
        let note = text("duplicate");
        let merge = |merge: Merge| store.merge(&actor, &merge).unwrap();

        merge(Merge {
            node: first_premise.into(),
            into: second_premise.into(),
            note: note.clone(),
        });
        merge(Merge {
            node: one.flaw.into(),
            into: two.flaw.into(),
            note: note.clone(),
        });
        assert_eq!(chain(&store, both), [two.flaw, between]);
        merge(Merge {
            node: one.candidate.into(),
            into: both.into(),
            note: note.clone(),
        });
        assert_eq!(actions(&store, both), [two.path.into(), one.path.into()]);
        assert_eq!(actions(&store, one.candidate), [one.path.into()]);
        let twice = New {
            title: text("raced"),
            body: text("the request races itself"),
            detail: NewCandidate {
                actions: vec![one.path, one.path].try_into().unwrap(),
                chain: vec![between].try_into().unwrap(),
            },
        };
        let twice: CandidateId = (store.upsert(&actor, &Upsert::Candidate(Change::Create(twice))))
            .unwrap()
            .get()
            .try_into()
            .unwrap();
        merge(Merge {
            node: twice.into(),
            into: both.into(),
            note: note.clone(),
        });
        assert_eq!(
            actions(&store, both),
            [two.path.into(), one.path.into(), one.path.into()]
        );
        let third = flaw(&store, &actor, two.component);
        let kept = store
            .report(&actor, third, &text("finding"), &text("report"))
            .unwrap();
        merge(Merge {
            node: two.flaw.into(),
            into: third.into(),
            note: note.clone(),
        });
        let refused = |node: NodeId, into: NodeId| {
            let merge = Merge {
                node,
                into,
                note: note.clone(),
            };
            store.merge(&actor, &merge).unwrap_err()
        };
        // A merge already made, here through `two.flaw`, is a no-op; a node itself, or one of
        // another kind, is refused.
        let before = last_event(&store);
        store
            .merge(
                &actor,
                &Merge {
                    node: one.flaw.into(),
                    into: third.into(),
                    note: note.clone(),
                },
            )
            .unwrap();
        assert_eq!(last_event(&store), before);
        assert_matches!(refused(third.into(), third.into()), Error::SelfMerge);
        let (path, component) = (two.path.into(), two.component.into());
        assert_matches!(
            refused(path, component),
            Error::MergeKinds { node, node_kind: NodeKind::Path, into, into_kind: NodeKind::Component }
                if node == path && into == component
        );
        store
            .evidence(
                &actor,
                one.flaw.into(),
                &[NewEvidence {
                    location: None,
                    environment: None,
                    stance: Stance::Supports,
                    note: text("seen"),
                }],
            )
            .unwrap();

        let view = |id: NodeId| store.node(id).unwrap();
        let NodeView::Path(path) = view(one.path.into()) else {
            panic!()
        };
        assert_eq!(
            path.premises
                .iter()
                .map(|premise| premise.id)
                .collect::<Vec<_>>(),
            [second_premise]
        );
        let NodeView::Premise(survivor) = view(second_premise.into()) else {
            panic!()
        };
        let ids = |premises: &[PremiseRef]| premises.iter().map(|p| p.id).collect::<Vec<_>>();
        assert_eq!(
            (ids(&survivor.rests_on), ids(&survivor.rested_on_by)),
            (vec![], vec![resting]),
            "a basis between the merged pair is dropped"
        );
        let NodeView::Flaw(flaw) = view(third.into()) else {
            panic!()
        };
        assert_eq!(
            flaw.premises
                .iter()
                .map(|premise| premise.id)
                .collect::<Vec<_>>(),
            [second_premise]
        );
        assert_eq!(
            flaw.candidates
                .iter()
                .map(|chained| (chained.id, chained.position))
                .collect::<Vec<_>>(),
            [(two.candidate, 0), (both, 0)]
        );
        assert_eq!(flaw.head.evidence.len(), 1);
        assert_eq!(chain(&store, both), [third, between]);
        let NodeView::Candidate(merged) = view(both.into()) else {
            panic!()
        };
        assert_eq!(
            merged
                .environments
                .iter()
                .filter_map(|env| env.candidate)
                .collect::<Vec<_>>(),
            [both]
        );
        assert_eq!(view(one.flaw.into()).head().merged_into, Some(third.into()));
        assert_eq!(view(rekeyed.into()).head().merged_into, Some(kept.into()));
        let findings = store.list(&ListQuery::Finding, Page::default()).unwrap();
        assert_eq!(
            findings
                .iter()
                .map(|finding| finding.id)
                .collect::<Vec<_>>(),
            [NodeId::from(kept)]
        );
    }

    /// A merged premise or candidate keeps the results its own work recorded as they stood,
    /// and its survivor inherits none of them. Work begun on a node merged away records its
    /// results on the survivor of the time, which keeps them once merged on in turn, whatever
    /// work on its own survivor records, and owns them on the next survivor from that merge on.
    #[test]
    fn a_merged_node_keeps_its_own_results() {
        let (_session, store, actor) = fixture();
        let (one, two) = (
            scenario(&store, &actor, "one"),
            scenario(&store, &actor, "two"),
        );
        let [merged, survivor, first, kept] =
            [(); 4].map(|_| premise(&store, &actor, PremiseScope::Code));
        decide(&store, merged, PremiseStatus::Holds);
        assess(&store, one.candidate, HIGH);
        verdict(&store, one.candidate, Verdict::Validated);
        let note = text("duplicate");
        let merges = [
            Merge {
                node: merged.into(),
                into: survivor.into(),
                note: note.clone(),
            },
            Merge {
                node: one.candidate.into(),
                into: two.candidate.into(),
                note,
            },
        ];
        for merge in &merges {
            store.merge(&actor, merge).unwrap();
        }
        let premise = |id: PremiseId| {
            let NodeView::Premise(view) = store.node(id.into()).unwrap() else {
                panic!("a premise")
            };
            (view.status, view.proposed.is_some())
        };
        assert_eq!(premise(merged), (PremiseStatus::Holds, false));
        assert_eq!(premise(survivor), (PremiseStatus::Undecided, false));
        let candidate = |id: CandidateId| {
            let NodeView::Candidate(view) = store.node(id.into()).unwrap() else {
                panic!("a candidate")
            };
            let proposed = view.proposed_assessment.is_some() || view.proposed_verdict.is_some();
            (view.assessment.is_some(), view.verdict.is_some(), proposed)
        };
        assert_eq!(candidate(one.candidate), (true, true, false));
        assert_eq!(candidate(two.candidate), (false, false, false));

        let [last, four] = ["three", "four"].map(|name| scenario(&store, &actor, name).candidate);
        let (settler, settle, _) = begun(&store, &Begin::Settle(vec![first]));
        let (reviewer, review, _) = begun(&store, &Begin::Review(vec![two.candidate]));
        let merge = |premise: PremiseId,
                     into: PremiseId,
                     candidate: CandidateId,
                     into_candidate: CandidateId| {
            let note = text("duplicate");
            let merges = [
                Merge {
                    node: premise.into(),
                    into: into.into(),
                    note: note.clone(),
                },
                Merge {
                    node: candidate.into(),
                    into: into_candidate.into(),
                    note,
                },
            ];
            for merge in &merges {
                store.merge(&actor, merge).unwrap();
            }
        };
        merge(first, kept, two.candidate, last);
        let holds = StatusChange::Premise(first, PremiseStatus::Holds);
        store.set_status(&settler, holds, &text("traced")).unwrap();
        let vector = HIGH.parse().unwrap();
        store
            .assess(&reviewer, two.candidate, vector, &text("scored"))
            .unwrap();
        store
            .verdict(
                &reviewer,
                two.candidate,
                Verdict::Validated,
                &text("reviewed"),
            )
            .unwrap();
        end_job(&store, review);
        merge(kept, survivor, last, four);
        let holds = StatusChange::Premise(first, PremiseStatus::Holds);
        store.set_status(&settler, holds, &text("traced")).unwrap();
        end_job(&store, settle);
        assert_eq!(premise(survivor), (PremiseStatus::Holds, false));
        decide(&store, survivor, PremiseStatus::Fails);
        assess(&store, four, HIGH);
        verdict(&store, four, Verdict::Disproved);
        assert_eq!(premise(kept), (PremiseStatus::Holds, false));
        assert_eq!(candidate(last), (true, true, false));
    }

    /// A task begun on a node merged away since records on the survivor as its own work, however
    /// many merges touch the retired node after its retirement: a flaw of its kept chain merged
    /// into one the survivor does not hold leaves it retired into the same survivor.
    #[test]
    fn a_retired_subjects_tasks_own_their_results_on_the_survivor() {
        let (_session, store, actor) = fixture();
        let (one, two) = (
            scenario(&store, &actor, "one"),
            scenario(&store, &actor, "two"),
        );
        let other = flaw(&store, &actor, one.component);
        let (reviewer, _, _) = begun(&store, &Begin::Review(vec![one.candidate]));
        let note = text("duplicate");
        let merges = [
            Merge {
                node: one.candidate.into(),
                into: two.candidate.into(),
                note: note.clone(),
            },
            Merge {
                node: one.flaw.into(),
                into: other.into(),
                note,
            },
        ];
        for merge in &merges {
            store.merge(&actor, merge).unwrap();
        }
        let vector = HIGH.parse().unwrap();
        store
            .assess(&reviewer, one.candidate, vector, &text("scored"))
            .unwrap();
        store
            .verdict(
                &reviewer,
                one.candidate,
                Verdict::Validated,
                &text("reviewed"),
            )
            .unwrap();
        let NodeView::Candidate(view) = store.node(two.candidate.into()).unwrap() else {
            panic!("a candidate")
        };
        assert!(view.assessment.is_some() && view.verdict.is_some());
        assert!(view.proposed_assessment.is_none() && view.proposed_verdict.is_none());
    }

    /// A merged path moves its accesses, premises and places in candidates' actions to the
    /// survivor, keeping its steps; a merged input state its accesses and the flaws leaving it
    /// wrong, freeing its location, which no other input state takes until then, for the
    /// survivor; and a merged component its files, interfaces, steps and flaws. Each leaves the
    /// sweep and probe queues and every coverage count, a merged path reaching no component and
    /// listed by none.
    #[test]
    fn a_merged_inventory_node_leaves_its_queues_and_counts() {
        let (_session, store, actor) = fixture();
        let (one, two) = (
            scenario(&store, &actor, "one"),
            scenario(&store, &actor, "two"),
        );
        let probes = [0x10, 0x100]
            .map(|at| path_from(&store, &actor, "daemon", &[two.component], daemon(at)));
        let relied = premise(&store, &actor, PremiseScope::Code);
        let link = Link::PathPremise {
            path: one.path,
            premise: relied,
        };
        store.link(&actor, &[link]).unwrap();
        let counts = || {
            let coverage = store.coverage().unwrap();
            let unreached = store.synthesis(&[]).unwrap().unreached;
            let counts = (coverage.states, coverage.paths, coverage.components);
            (counts, coverage.unreached, unreached)
        };
        assert_eq!(counts(), ((4, 4, 2), 0, vec![]));
        let note = text("duplicate");
        let merge = |merge: Merge| store.merge(&actor, &merge).unwrap();
        merge(Merge {
            node: one.path.into(),
            into: two.path.into(),
            note: note.clone(),
        });
        merge(Merge {
            node: probes[1].into(),
            into: probes[0].into(),
            note: note.clone(),
        });
        assert_eq!(queued(&store, Phase::Sweep), [NodeId::from(two.path)]);
        assert_eq!(queued(&store, Phase::Probe), [NodeId::from(probes[0])]);
        assert_eq!(counts(), ((4, 2, 2), 1, vec![one.component]));
        assert_eq!(actions(&store, one.candidate), [two.path.into()]);
        assert_eq!(steps(&store, one.path), [(0, one.component)]);
        let NodeView::Path(survivor) = store.node(two.path.into()).unwrap() else {
            panic!("a path")
        };
        assert_eq!(
            survivor.premises.iter().map(|p| p.id).collect::<Vec<_>>(),
            [relied]
        );
        let [merged_input, kept_input] = [0, 1].map(|at| survivor.states[at].id);
        let [freed, held] = [0, 1].map(|at| survivor.states[at].location.clone());
        let wrong = Link::FlawState {
            flaw: one.flaw,
            state: merged_input,
        };
        store.link(&actor, &[wrong]).unwrap();
        let adopt = |location: &Location| {
            let edit = Edit {
                title: None,
                body: None,
                detail: StateEdit {
                    location: Some(location.clone()),
                    trust: None,
                },
            };
            store.upsert(&actor, &Upsert::State(Change::Edit(kept_input, edit)))
        };
        assert_matches!(
            adopt(&freed),
            Err(Error::StateAt(at)) if at == merged_input
        );
        merge(Merge {
            node: merged_input.into(),
            into: kept_input.into(),
            note: note.clone(),
        });
        let NodeView::State(merged) = store.node(merged_input.into()).unwrap() else {
            panic!("a state")
        };
        assert_eq!(
            (merged.head.merged_into, merged.input.is_none()),
            (Some(kept_input.into()), true)
        );
        adopt(&freed).unwrap();
        merge(Merge {
            node: one.component.into(),
            into: two.component.into(),
            note,
        });
        assert_eq!(counts(), ((3, 2, 1), 0, vec![]));
        let NodeView::State(kept) = store.node(kept_input.into()).unwrap() else {
            panic!("a state")
        };
        assert_eq!(
            (kept.accesses.iter())
                .map(|access| (access.path.id, access.location.clone()))
                .collect::<Vec<_>>(),
            [(two.path.into(), freed.clone()), (two.path.into(), held)]
        );
        assert_eq!(kept.input.unwrap().location, freed);
        assert_eq!(
            kept.flaws.iter().map(|flaw| flaw.id).collect::<Vec<_>>(),
            [one.flaw]
        );
        let NodeView::Component(component) = store.node(two.component.into()).unwrap() else {
            panic!("a component")
        };
        assert_eq!(component.files, ["one/lib.c", "two/lib.c"]);
        assert_eq!(component.interfaces.len(), 2);
        assert_eq!(
            (component.paths.iter().map(|path| path.id)).collect::<Vec<_>>(),
            [NodeId::from(two.path), probes[0].into()],
            "a merged path's steps are history"
        );
        assert_eq!(
            component
                .flaws
                .iter()
                .map(|flaw| flaw.id)
                .collect::<Vec<_>>(),
            [one.flaw, two.flaw]
        );
        assert_eq!(steps(&store, one.path), [(0, two.component)]);
    }
}

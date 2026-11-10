//! Edges linked and unlinked, and merges of duplicate nodes, which move every edge of the merged
//! node to the node it is merged into. A candidate's chain and actions are set by its upsert or a
//! merge, rewritten past the prefix they keep, or one flaw replaced in place.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::Deserialize;
use skyhook::db::{Db, params};

use crate::store::*;

/// An edge between two nodes
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Edge {
    /// Where the path's flow reads or writes the state
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
    /// The premise holds only if `rests_on` does
    PremiseBasis {
        premise: PremiseId,
        rests_on: PremiseId,
    },
    /// The flaw leaves the state wrong
    FlawState {
        flaw: FlawId,
        state: StateId,
    },
}

/// Whether a write adds edges or removes them.
#[derive(Clone, Copy)]
enum Verb {
    Link,
    Unlink,
}

/// The statements adding and removing one kind of edge between two nodes `?1` and `?2`, and how
/// an edit of one bears on its first end.
struct Pair {
    link: &'static str,
    unlink: &'static str,
    effect: EventEffect,
}

macro_rules! pair {
    ($table:literal, $first:literal, $second:literal, $effect:ident) => {
        Pair {
            link: concat!(
                "INSERT INTO ",
                $table,
                "(",
                $first,
                ",",
                $second,
                ") VALUES (?1,?2) ON CONFLICT DO NOTHING"
            ),
            unlink: concat!(
                "DELETE FROM ",
                $table,
                " WHERE ",
                $first,
                "=?1 AND ",
                $second,
                "=?2"
            ),
            effect: EventEffect::$effect,
        }
    };
}

const PATH_PREMISE: Pair = pair!("path_premise", "path", "premise", Standing);
const FLAW_PREMISE: Pair = pair!("flaw_premise", "flaw", "premise", Standing);
const PREMISE_BASIS: Pair = pair!("premise_basis", "premise", "rests_on", Concerns);
const FLAW_STATE: Pair = pair!("flaw_state", "flaw", "state", Standing);

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

fn edge(db: &Db, verb: Verb, edge: &Edge, marks: &mut Marks) -> Result<()> {
    let (pair, first, second): (_, NodeId, NodeId) = match *edge {
        Edge::PathState {
            path,
            state,
            access,
            ref location,
        } => {
            let (path, state) = (live(db, path)?, live(db, state)?);
            let sql = match verb {
                Verb::Link => {
                    "INSERT INTO path_state(path,state,access,location) VALUES (?1,?2,?3,?4) ON \
                     CONFLICT DO NOTHING"
                }
                Verb::Unlink => {
                    "DELETE FROM path_state WHERE path=?1 AND state=?2 AND access=?3 AND \
                     location=?4"
                }
            };
            let at = location_id(db, location)?;
            let edited = db.execute(sql, params![path, state, access, at])? > 0;
            accessed(marks, edited, path, state);
            return Ok(());
        }
        Edge::PathPremise { path, premise } => (
            &PATH_PREMISE,
            live(db, path)?.into(),
            live(db, premise)?.into(),
        ),
        Edge::FlawPremise { flaw, premise } => (
            &FLAW_PREMISE,
            live(db, flaw)?.into(),
            live(db, premise)?.into(),
        ),
        Edge::PremiseBasis { premise, rests_on } => {
            let (premise, rests_on) = basis_ends(db, premise, rests_on)?;
            (&PREMISE_BASIS, premise.into(), rests_on.into())
        }
        Edge::FlawState { flaw, state } => {
            (&FLAW_STATE, live(db, flaw)?.into(), live(db, state)?.into())
        }
    };
    let sql = match verb {
        Verb::Link => pair.link,
        Verb::Unlink => pair.unlink,
    };
    let edited = db.execute(sql, params![first, second])? > 0;
    ends(marks, edited, first, pair.effect, second);
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

/// Fold duplicate `node`, of any kind but a finding, into `into`, a node of its kind: its edges
/// move to `into`
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Merge {
    pub(crate) node: NodeId,
    pub(crate) into: NodeId,
    pub(crate) note: Text,
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
    pub(crate) fn link(&self, actor: &AgentId, edges: &[Edge]) -> Result<()> {
        self.edges(actor, Verb::Link, edges)
    }

    /// Remove edges, as one event; an edge not there is restated absent.
    pub(crate) fn unlink(&self, actor: &AgentId, edges: &[Edge]) -> Result<()> {
        self.edges(actor, Verb::Unlink, edges)
    }

    fn edges(&self, actor: &AgentId, verb: Verb, edges: &[Edge]) -> Result<()> {
        let kind = match verb {
            Verb::Link => EventKind::Linked,
            Verb::Unlink => EventKind::Unlinked,
        };
        self.write(|db| {
            audit(db, actor, kind, None, |_| {
                let mut marks = Marks::default();
                for each in edges {
                    edge(db, verb, each, &mut marks)?;
                }
                Ok(((), marks))
            })
        })
    }

    /// Merge a duplicate node into a live node of its kind, with the actor's reason; ids of
    /// merged nodes name the node they were merged into, so two already one are left as they are.
    pub(crate) fn merge(&self, actor: &AgentId, merge: &Merge) -> Result<()> {
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

    use super::*;
    use crate::store::tests::{
        HIGH, assess, begun, candidate, component, daemon, decide, end_job, fixture, flaw,
        last_event, marks, path, path_from, premise, restep, scenario, text, verdict,
    };
    use crate::store::work::queues::tests::queued;

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

    /// A path's steps and a candidate's chain are set whole, in order at positions 0..n-1: the
    /// components joining or leaving the steps are concerned, and the same steps again only
    /// concern the path. A chain holds each flaw once, and a candidate's root is its first flaw.
    #[test]
    fn ordered_lists_are_set_whole() {
        let (_session, store, actor) = fixture();
        let [a, b, c] = ["a", "b", "c"].map(|name| component(&store, &actor, name));
        let route = path(&store, &actor, "route", &[a, b]);
        let through = [(c, "enters"), (a, "calls"), (b, "calls"), (a, "returns")];
        let effects = || marks(&store, last_event(&store));
        let (changed, concerns) = (EventEffect::Changed, EventEffect::Concerns);
        restep(&store, &actor, route, &through);
        assert_eq!(steps(&store, route), [(0, c), (1, a), (2, b), (3, a)]);
        assert_eq!(effects(), [(c.into(), concerns), (route.into(), changed)]);
        restep(&store, &actor, route, &through);
        assert_eq!(
            effects(),
            [(route.into(), concerns)],
            "the same steps are restated"
        );
        restep(&store, &actor, route, &[(b, "calls")]);
        assert_eq!(steps(&store, route), [(0, b)]);
        assert_eq!(
            effects(),
            [
                (a.into(), concerns),
                (c.into(), concerns),
                (route.into(), changed)
            ]
        );

        let [first, second, third] = [(); 3].map(|_| flaw(&store, &actor, a));
        let id = candidate(&store, &actor, route, &[third, first, second, first]);
        assert_eq!(chain(&store, id), [third, first, second]);
        let rooted = |root| {
            let query = ListQuery::Candidate {
                root: Some(root),
                state: None,
            };
            let listed = store.list(&query, Page::default()).unwrap();
            listed.iter().map(Keyed::key).collect::<Vec<_>>()
        };
        assert_eq!(rooted(third), [id.0]);
        let edit = CandidateEdit {
            id,
            title: None,
            body: None,
            actions: None,
            chain: Some(vec![first].try_into().unwrap()),
        };
        let change = Upsert::Edit(NodeEdit::Candidate(edit));
        store.upsert(&actor, &change).unwrap();
        assert_eq!(chain(&store, id), [first]);
        assert_eq!(rooted(first), [id.0]);
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
        let rests = |premise, rests_on| Edge::PremiseBasis { premise, rests_on };
        assert_matches!(
            store.link(&actor, &[rests(resting, resting)]),
            Err(Error::SelfBasis)
        );
        let links = [
            rests(first_premise, second_premise),
            rests(resting, first_premise),
            Edge::FlawPremise {
                flaw: one.flaw,
                premise: first_premise,
            },
            Edge::FlawPremise {
                flaw: two.flaw,
                premise: second_premise,
            },
            Edge::PathPremise {
                path: one.path,
                premise: first_premise,
            },
            Edge::PathPremise {
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
        let twice = NewCandidate {
            title: text("raced"),
            body: text("the request races itself"),
            actions: vec![one.path, one.path].try_into().unwrap(),
            chain: vec![between].try_into().unwrap(),
        };
        let twice: CandidateId = (store.upsert(&actor, &Upsert::Create(NewNode::Candidate(twice))))
            .unwrap()
            .0
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
        let findings = store.list(&ListQuery::Finding {}, Page::default()).unwrap();
        assert_eq!(
            findings.iter().map(Keyed::key).collect::<Vec<_>>(),
            [kept.0]
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
        let (settler, settle, _) = begun(&store, &Begin::Settle(Listed::of([first])));
        let (reviewer, review, _) = begun(&store, &Begin::Review(Listed::of([two.candidate])));
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
        let holds = StatusChange::Premise {
            id: first,
            status: PremiseStatus::Holds,
        };
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
        let holds = StatusChange::Premise {
            id: first,
            status: PremiseStatus::Holds,
        };
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
        let (reviewer, _, _) = begun(&store, &Begin::Review(Listed::of([one.candidate])));
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
        let link = Edge::PathPremise {
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
        let wrong = Edge::FlawState {
            flaw: one.flaw,
            state: merged_input,
        };
        store.link(&actor, &[wrong]).unwrap();
        let adopt = |location: &Location| {
            let edit = StateEdit {
                id: kept_input,
                title: None,
                body: None,
                location: Some(location.clone()),
                trust: None,
            };
            store.upsert(&actor, &Upsert::Edit(NodeEdit::State(edit)))
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
            (merged.head.merged_into, merged.scope),
            (
                Some(kept_input.into()),
                StateScoped::Input(LiveInput { read: None })
            )
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
        let StateScoped::Input(LiveInput { read: Some(read) }) = &kept.scope else {
            panic!("a live input state")
        };
        assert_eq!(read.location, freed);
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

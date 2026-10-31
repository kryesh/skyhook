//! Edges linked and unlinked, and merges of duplicate premises, flaws and candidates, which
//! move every edge of the merged node to the node it is merged into. Ordered edges, a path's
//! steps and a candidate's chain, keep positions 0..n-1 through every write; a chain is edited
//! whole, by its candidate's upsert.

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
    PathSource {
        path: PathId,
        source: SourceId,
    },
    PathPremise {
        path: PathId,
        premise: PremiseId,
    },
    FlawPremise {
        flaw: FlawId,
        premise: PremiseId,
    },
}

/// An edge to remove; a path's step by its position.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Unlink {
    PathStep { path: PathId, position: u32 },
    PathSource { path: PathId, source: SourceId },
    PathPremise { path: PathId, premise: PremiseId },
    FlawPremise { flaw: FlawId, premise: PremiseId },
}

/// The statements keeping one ordered edge table's positions contiguous for an owner `?1`:
/// how many it has, and the two-step shifts opening a gap at `?2` or closing the one at
/// `?2`. Each shift first lifts the moving rows past every position in use, so no step
/// collides with a row not yet moved.
struct Ordered {
    count: &'static str,
    open: [&'static str; 2],
    close: [&'static str; 2],
}

macro_rules! ordered {
    ($table:literal, $owner:literal) => {
        Ordered {
            count: concat!("SELECT count(*) FROM ", $table, " WHERE ", $owner, "=?1"),
            open: [
                concat!(
                    "UPDATE ",
                    $table,
                    " SET position=position+4294967296 WHERE ",
                    $owner,
                    "=?1 AND position>=?2"
                ),
                concat!(
                    "UPDATE ",
                    $table,
                    " SET position=position-4294967295 WHERE ",
                    $owner,
                    "=?1 AND position>=4294967296"
                ),
            ],
            close: [
                concat!(
                    "UPDATE ",
                    $table,
                    " SET position=position+4294967296 WHERE ",
                    $owner,
                    "=?1 AND position>?2"
                ),
                concat!(
                    "UPDATE ",
                    $table,
                    " SET position=position-4294967297 WHERE ",
                    $owner,
                    "=?1 AND position>=4294967296"
                ),
            ],
        }
    };
}

const PATH_STEPS: Ordered = ordered!("path_step", "path");
const CHAIN: Ordered = ordered!("candidate_flaw", "candidate");

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
        Link::PathSource { path, source } => {
            let (path, source) = (live(db, *path)?, live(db, *source)?);
            let sql = "INSERT INTO path_source(path,source) VALUES (?1,?2) ON CONFLICT DO NOTHING";
            let added = db.execute(sql, params![path, source])? > 0;
            ends(marks, added, path, EventEffect::Changed, source);
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
    }
    Ok(())
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
        Unlink::PathSource { path, source } => {
            let (path, source) = (live(db, *path)?, live(db, *source)?);
            let sql = "DELETE FROM path_source WHERE path=?1 AND source=?2";
            let removed = db.execute(sql, params![path, source])? > 0;
            ends(marks, removed, path, EventEffect::Changed, source);
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
    }
    Ok(())
}

/// A duplicate folded into `into`, a node of its kind, with the actor's reason.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
pub enum Merge {
    Premise {
        premise: PremiseId,
        into: PremiseId,
        note: Text,
    },
    Flaw {
        flaw: FlawId,
        into: FlawId,
        note: Text,
    },
    Candidate {
        candidate: CandidateId,
        into: CandidateId,
        note: Text,
    },
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

/// `from`'s evidence moves to `into`, and `from` and what was merged into it point at `into`.
fn absorb(db: &Db, from: NodeId, into: NodeId) -> Result<()> {
    db.execute(
        "UPDATE evidence SET node=?2 WHERE node=?1",
        params![from, into],
    )?;
    db.execute(
        "UPDATE node SET merged_into=?2 WHERE id=?1 OR merged_into=?1",
        params![from, into],
    )?;
    Ok(())
}

/// Flaw `from`'s places in chains go to `into`: a chain holding both keeps the earlier place,
/// so its root stays the flaw it was or the one that flaw became. The root's finding follows
/// it, or is absorbed by `into`'s.
fn rechain(db: &Db, from: FlawId, into: FlawId, marks: &mut Marks) -> Result<()> {
    let held = rows(
        db,
        "SELECT candidate FROM candidate_flaw WHERE flaw=?1",
        params![from],
        |row| Ok(NodeId(row.get(0)?)),
    )?;
    for candidate in held {
        marks.mark(candidate, EventEffect::Standing);
    }
    let later = rows(
        db,
        "SELECT a.candidate,max(a.position,b.position) FROM candidate_flaw a JOIN candidate_flaw \
         b ON b.candidate=a.candidate AND b.flaw=?2 WHERE a.flaw=?1",
        params![from, into],
        |row| Ok((NodeId(row.get(0)?), row.get::<i64>(1)?)),
    )?;
    for (candidate, position) in later {
        db.execute(
            "DELETE FROM candidate_flaw WHERE candidate=?1 AND position=?2",
            params![candidate, position],
        )?;
        CHAIN.close(db, candidate, position)?;
    }
    db.execute(
        "UPDATE candidate_flaw SET flaw=?2 WHERE flaw=?1",
        params![from, into],
    )?;
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
            absorb(db, absorbed, kept)?;
            marks.mark(absorbed, EventEffect::Changed);
            marks.mark(kept, EventEffect::Changed);
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

/// The live ends of a merge of two nodes of `I`'s kind.
fn pair<I: KindId>(db: &Db, from: I, into: I) -> Result<(I, I)> {
    let (from, into) = (live(db, from)?, live(db, into)?);
    if Into::<NodeId>::into(from) == into.into() {
        return Err(Error::SelfMerge);
    }
    Ok((from, into))
}

/// Fold `merge`'s duplicate into its survivor: every edge naming it names the survivor,
/// marking the nodes at their other ends. A merged candidate keeps its chain, tests and
/// verdict as history, and leaves its root's group as a candidate losing its root does.
fn fold(db: &Db, merge: &Merge, marks: &mut Marks) -> Result<(NodeId, NodeId)> {
    Ok(match merge {
        Merge::Premise { premise, into, .. } => {
            let (from, into) = pair(db, *premise, *into)?;
            let edges = [
                repoint!("flaw_premise", "premise", "flaw", duplicates),
                repoint!("path_premise", "premise", "path", duplicates),
            ];
            repoint(db, &edges, from.into(), into.into(), marks)?;
            (from.into(), into.into())
        }
        Merge::Flaw { flaw, into, .. } => {
            let (from, into) = pair(db, *flaw, *into)?;
            let edges = [repoint!("flaw_premise", "flaw", "premise", duplicates)];
            repoint(db, &edges, from.into(), into.into(), marks)?;
            rechain(db, from, into, marks)?;
            (from.into(), into.into())
        }
        Merge::Candidate {
            candidate, into, ..
        } => {
            let (from, into) = pair(db, *candidate, *into)?;
            let edges = [repoint!("environment", "candidate", "NULL")];
            repoint(db, &edges, from.into(), into.into(), marks)?;
            let root = optional(
                db,
                "SELECT root FROM candidate_root WHERE candidate=?1",
                params![from],
                |row| Ok(FlawId(row.get(0)?)),
            )?;
            marks.rerooted(root, None);
            (from.into(), into.into())
        }
    })
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

    /// Merge a duplicate premise, flaw or candidate into a live node of its kind, with the
    /// actor's reason; ids of merged nodes name the node they were merged into.
    pub fn merge(&self, actor: &AgentId, merge: &Merge) -> Result<()> {
        let (Merge::Premise { note, .. }
        | Merge::Flaw { note, .. }
        | Merge::Candidate { note, .. }) = merge;
        self.write(|db| {
            audit(db, actor, EventKind::Merged, Some(note), |_| {
                let mut marks = Marks::default();
                let (from, into) = fold(db, merge, &mut marks)?;
                absorb(db, from, into)?;
                marks.mark(from, EventEffect::Changed);
                marks.mark(into, EventEffect::Changed);
                Ok(((), marks))
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::tests::{
            candidate, component, fixture, flaw, last_event, marks, path, premise, scenario, text,
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
                path: None,
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
    /// became, with the root's finding re-keyed or absorbed; and `merged_into` stays one hop:
    /// writes naming a merged node land on the survivor.
    #[test]
    fn a_merge_moves_every_edge_to_the_survivor() {
        let (_session, store, actor) = fixture();
        let (one, two) = (
            scenario(&store, &actor, "one"),
            scenario(&store, &actor, "two"),
        );
        let [first_premise, second_premise] =
            [(); 2].map(|_| premise(&store, &actor, PremiseScope::Code));
        let links = [
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
        };
        store.environment(&actor, &environment).unwrap();
        let note = text("duplicate");
        let merge = |merge: Merge| store.merge(&actor, &merge).unwrap();

        merge(Merge::Premise {
            premise: first_premise,
            into: second_premise,
            note: note.clone(),
        });
        merge(Merge::Flaw {
            flaw: one.flaw,
            into: two.flaw,
            note: note.clone(),
        });
        assert_eq!(chain(&store, both), [two.flaw, between]);
        merge(Merge::Candidate {
            candidate: one.candidate,
            into: both,
            note: note.clone(),
        });
        let third = flaw(&store, &actor, two.component);
        let kept = store
            .report(&actor, third, &text("finding"), &text("report"))
            .unwrap();
        merge(Merge::Flaw {
            flaw: two.flaw,
            into: third,
            note: note.clone(),
        });
        let refused = |flaw, into| {
            let merge = Merge::Flaw {
                flaw,
                into,
                note: note.clone(),
            };
            store.merge(&actor, &merge).unwrap_err()
        };
        assert!(matches!(refused(third, one.flaw), Error::SelfMerge));
        assert!(matches!(
            refused(third, FlawId(two.path.0)),
            Error::WrongKind { .. }
        ));
        store
            .evidence(
                &actor,
                one.flaw.into(),
                &[NewEvidence {
                    location: None,
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
}

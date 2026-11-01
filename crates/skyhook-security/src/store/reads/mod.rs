//! Reads: one node with its neighbourhood, keyset listings of live nodes, and environments.
//! A read names the node asked for, merged or not; its view says what it was merged into.

use skyhook::db::{enum_column, params};

use self::neighbours::*;

use super::*;

mod list;
mod neighbours;
mod shelf;
mod views;

pub use list::*;
pub(super) use neighbours::{disputed, undecided_premises};
pub use shelf::*;
pub use views::*;

impl Store {
    /// The node `id`, merged or not, with its neighbourhood.
    pub fn node(&self, id: NodeId) -> Result<NodeView> {
        self.read(|db| {
            let (kind, head) = optional(
                db,
                "SELECT kind,title,body,merged_into FROM node WHERE id=?1",
                params![id],
                |row| {
                    Ok((
                        enum_column::<NodeKind>(row, 0)?,
                        Head {
                            id,
                            title: row.get(1)?,
                            body: row.get(2)?,
                            merged_into: row.get::<Option<i64>>(3)?.map(NodeId),
                            evidence: evidence(db, id)?,
                            tasks: tasks::of_node(db, id)?,
                        },
                    ))
                },
            )?
            .ok_or(Error::NotFound {
                record: Record::Node(None),
                id: Some(id.0),
            })?;
            Ok(match kind {
                NodeKind::Component => NodeView::Component(component(db, head)?),
                NodeKind::Path => NodeView::Path(path(db, head)?),
                NodeKind::Source => NodeView::Source(source(db, head)?),
                NodeKind::Premise => NodeView::Premise(premise(db, head)?),
                NodeKind::Flaw => NodeView::Flaw(flaw(db, head)?),
                NodeKind::Candidate => NodeView::Candidate(candidate(db, head)?),
                NodeKind::Finding => NodeView::Finding(finding(db, head)?),
            })
        })
    }

    /// A page of the run's environments, oldest first.
    pub fn environments(&self, page: Page) -> Result<Listing<EnvironmentView>> {
        self.read(|db| {
            let items = environments(
                db,
                "SELECT id,instructions,candidate,event FROM environment WHERE id>?1 ORDER BY \
                 id LIMIT ?2",
                params![page.after, page.limit()],
            )?;
            Ok(Listing::keyset(items, page))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::tests::{begun, fixture, premise, scenario, text},
        *,
    };

    /// A node's view lists its tasks newest first, and a merge's survivor lists the merged
    /// node's too.
    #[test]
    fn a_node_lists_its_tasks_through_a_merge() {
        let (_session, store, actor) = fixture();
        let (merged, kept) = (
            scenario(&store, &actor, "one"),
            scenario(&store, &actor, "two"),
        );
        let (_, _, first) = begun(&store, &Begin::Assess(vec![merged.candidate]));
        let (_, _, second) = begun(&store, &Begin::Assess(vec![kept.candidate]));
        let merge = Merge::Candidate {
            candidate: merged.candidate,
            into: kept.candidate,
            note: text("same"),
        };
        store.merge(&actor, &merge).unwrap();
        let tasks = |id: CandidateId| store.node(id.into()).unwrap().head().tasks.clone();
        assert_eq!(tasks(kept.candidate), [second, first]);
        assert_eq!(tasks(merged.candidate), [first]);
    }

    /// Evidence recorded on a node of any kind shows in its view.
    #[test]
    fn every_kind_shows_its_evidence() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let NodeView::Path(path) = store.node(scenario.path.into()).unwrap() else {
            panic!("a path")
        };
        let finding = store
            .report(&actor, scenario.flaw, &text("overflow"), &text("report"))
            .unwrap();
        let nodes: [NodeId; 7] = [
            scenario.component.into(),
            scenario.path.into(),
            path.sources[0].id.into(),
            premise(&store, &actor, PremiseScope::Code).into(),
            scenario.flaw.into(),
            scenario.candidate.into(),
            finding.into(),
        ];
        for node in nodes {
            let item = NewEvidence {
                location: None,
                environment: None,
                stance: Stance::Context,
                note: text("seen"),
            };
            store.evidence(&actor, node, &[item]).unwrap();
            let view = serde_json::to_value(store.node(node).unwrap()).unwrap();
            assert_eq!(view["evidence"][0]["note"], "seen", "{}", view["kind"]);
        }
    }
}

//! Reads: one node with its neighbourhood, keyset listings of live nodes, and environments.
//! A read names the node asked for, merged or not; its view says what it was merged into.

use skyhook::db::{enum_column, params};

use self::neighbours::*;

use super::*;

mod list;
mod neighbours;
mod page;
mod reporting;
mod search;
mod shelf;
mod views;

pub(crate) use list::*;
pub(super) use neighbours::{decided_premises, proposed, undecided_premises};
pub(crate) use page::*;
pub(crate) use reporting::*;
pub(crate) use search::*;
pub(crate) use shelf::*;
pub(crate) use views::*;

impl Store {
    /// The node `id`, merged or not, with its neighbourhood.
    pub(crate) fn node(&self, id: NodeId) -> Result<NodeView> {
        let session = self.id();
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
                NodeKind::State => NodeView::State(state(db, head)?),
                NodeKind::Premise => NodeView::Premise(premise(db, session, head)?),
                NodeKind::Flaw => NodeView::Flaw(flaw(db, session, head)?),
                NodeKind::Candidate => NodeView::Candidate(candidate(db, session, head)?),
                NodeKind::Finding => NodeView::Finding(finding(db, head)?),
            })
        })
    }

    /// A page of the run's environments, oldest first.
    pub(crate) fn environments(&self, page: Page) -> Result<Listing<EnvironmentView>> {
        self.read(|db| {
            let items = environments(
                db,
                "SELECT id,instructions,candidate,event FROM environment WHERE id>?1 ORDER BY id",
                params![page.after],
                page.limit(),
            )?;
            Ok(Listing::keyset(items, page))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::{
        access, check, component, fixture, location, path, premise, scenario, state, sweep, text,
    };

    /// A state's check stands until an access, edit or merge comes after it, whether or not the
    /// state is still owed one: retracting the state leaves a stale check stale.
    #[test]
    fn a_checks_currency_outlives_its_states_retraction() {
        let (_session, store, actor) = fixture();
        let decoder = component(&store, &actor, "decoder");
        let stream = path(&store, &actor, "stream", &[decoder]);
        let window = state(&store, &actor, NewState::Instance);
        let write = |line| {
            access(
                stream,
                window,
                Access::Write,
                location("decoder/lib.c", line),
            )
        };
        store.relink(&actor, &[write(3)], &[]).unwrap();
        sweep(&store, stream);
        let basis = premise(&store, &actor, PremiseScope::Code);
        check(&store, window, &[basis]);
        let current = || {
            let NodeView::State(view) = store.node(window.into()).unwrap() else {
                panic!("a state")
            };
            view.check.expect("a check").current
        };
        assert!(current());
        store.relink(&actor, &[write(4)], &[]).unwrap();
        assert!(!current());
        let retracted = StatusChange::State(StateMark::Retracted { id: window });
        store.set_status(&actor, retracted, &text("gone")).unwrap();
        assert!(!current());
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
            path.states[0].id.into(),
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

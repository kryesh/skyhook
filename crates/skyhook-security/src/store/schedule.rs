//! What the orchestrator's scheduler starts next: the offered queue entries in pipeline
//! order, leaving out what is running, what running work holds back, later-stage work a ready
//! earlier-stage premise or flaw holds back, and the tests it may not start yet, a page at a
//! time. Premises and flaws come with the ready entries of their phase sharing a node with
//! them, which go to one agent.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::{
    Bounded,
    db::{Db, params},
};

use super::*;

/// Where a page of startable work continues: after key `after` of `phase`'s queue.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub phase: Phase,
    pub after: i64,
}

/// An entry to start now, with the ready entries its agent takes too.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Startable {
    pub phase: Phase,
    #[serde(flatten)]
    pub entry: QueueEntry,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bundle: Vec<QueueEntry>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Startables {
    pub items: Vec<Startable>,
    /// Where the next page continues; none at the end.
    pub next: Option<Cursor>,
    /// A test the page passed over waits for an approval or an unblock.
    pub held: bool,
}

/// The tests the launch's live policy leaves to the user that may start now; the rest wait.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Live {
    /// Candidates the user approved a test of, under the launch's `ask` policy.
    #[serde(default)]
    pub approved: Vec<CandidateId>,
    /// Pending tests whose blocker the user resolved: each admits only the resumption of the
    /// pending test it names, while that test is its candidate's latest.
    #[serde(default)]
    pub unblocked: Vec<TestId>,
}

impl Live {
    /// Whether test `entry` may start under `policy`: it needs the user's approval under `ask`,
    /// and a pending one its blocker resolved.
    fn admits(&self, policy: LivePolicy, entry: &QueueEntry) -> bool {
        match entry.pending {
            Some(pending) => self.unblocked.contains(&pending),
            None => policy == LivePolicy::Yes || self.approved.contains(&CandidateId(entry.node.0)),
        }
    }
}

/// Work that holds other work back: what is running or about to start. Bundled work and the
/// work sharing a node with it hold each other.
struct Work {
    node: NodeId,
    bundled: bool,
    about: Vec<NodeId>,
}

impl Work {
    fn of(phase: Phase, entry: &QueueEntry) -> Self {
        Self {
            node: entry.node,
            bundled: phase.bundled(),
            about: entry.about.clone(),
        }
    }

    /// Live tasks' subjects, and nodes the scheduler launched work on, each merged one as the
    /// node it was merged into, which count as bundled work when a bundled phase works their
    /// kind.
    fn running(db: &Db, live: &[(Phase, NodeId)], launched: &[NodeId]) -> Result<Vec<Self>> {
        let launched = rows(
            db,
            "SELECT coalesce(n.merged_into,n.id) FROM json_each(?1) j JOIN node n ON n.id=j.value \
             ORDER BY j.key",
            params![json_list(launched)?],
            |row| Ok(NodeId(row.get(0)?)),
        )?;
        let nodes: Vec<NodeId> = (live.iter().map(|(_, node)| *node))
            .chain(launched.iter().copied())
            .collect();
        let about = queues::about(db, &nodes)?;
        let work = |node: NodeId, phase: Option<Phase>| {
            let (bundled, about) = match about.get(&node) {
                Some((kind, about)) => {
                    let bundled = match phase {
                        Some(phase) => phase.bundled(),
                        None => (Phase::ALL.iter())
                            .any(|phase| phase.bundled() && phase.subject() == *kind),
                    };
                    (bundled, about.clone())
                }
                None => (false, Vec::new()),
            };
            Self {
                node,
                bundled,
                about,
            }
        };
        Ok((live.iter())
            .map(|(phase, node)| work(*node, Some(*phase)))
            .chain(launched.iter().map(|node| work(*node, None)))
            .collect())
    }

    fn holds(&self, phase: Phase, entry: &QueueEntry) -> bool {
        self.node == entry.node
            || ((self.bundled || phase.bundled())
                && self.about.iter().any(|about| entry.about.contains(about)))
    }
}

impl Store {
    /// The next page of work that can start now, earliest pipeline stage first: what the
    /// queues offer (no live task, not exhausted, counting `spent` work exhausted while owed at
    /// its basis), none of `running` (launched, perhaps not yet begun) or held back by it or by
    /// live tasks, nothing a ready earlier-stage bundled entry touches, and no test beyond what
    /// `live` admits. A premise or flaw carries every other such entry of its phase sharing a
    /// node with it, and what one page starts holds back the rest of it as running work would.
    /// Reading reserves nothing.
    pub fn startable(
        &self,
        running: &[NodeId],
        spent: &[SpentWork],
        live: &Live,
        after: Option<Cursor>,
        limit: Bounded<1, 1000>,
    ) -> Result<Startables> {
        let mut phases = Phase::ALL.to_vec();
        phases.sort_by_key(|phase| phase.stage());
        let first = match after {
            Some(cursor) => phases.iter().position(|phase| *phase == cursor.phase),
            None => Some(0),
        };
        let wanted = u64::from(limit) as usize;
        let spent = &queues::Spent::of(spent)?;
        self.read(|db| {
            let policy = runs::run(db)?.live;
            let mut busy = Work::running(db, &queues::live_subjects(db)?, running)?;
            let (mut items, mut held) = (Vec::new(), false);
            for &phase in phases.iter().skip(first.unwrap_or(phases.len())) {
                let earlier: Vec<Phase> = (phases.iter().copied())
                    .filter(|earlier| earlier.bundled() && earlier.stage() < phase.stage())
                    .collect();
                // Tests are paged whole: the ones `live` admits may lie far behind the rest.
                let mut page = Page::new(
                    after
                        .filter(|cursor| cursor.phase == phase)
                        .map_or(0, |cursor| cursor.after),
                    if phase == Phase::Test {
                        Bounded::new::<1000>()
                    } else {
                        limit
                    },
                );
                loop {
                    let listing = queues::page(db, phase, page, spent)?;
                    let touched = touched(db, &listing, &earlier, spent)?;
                    for entry in listing.items {
                        let key = entry.key();
                        if busy.iter().any(|work| work.holds(phase, &entry))
                            || entry.about.iter().any(|about| touched.contains(about))
                        {
                            continue;
                        }
                        if phase == Phase::Test && !live.admits(policy, &entry) {
                            held = true;
                            continue;
                        }
                        let bundle = match phase.bundled() {
                            true => partners(db, phase, &entry, &busy, spent)?,
                            false => Vec::new(),
                        };
                        busy.push(Work::of(phase, &entry));
                        busy.extend(bundle.iter().map(|entry| Work::of(phase, entry)));
                        items.push(Startable {
                            phase,
                            entry,
                            bundle,
                        });
                        if items.len() == wanted {
                            return Ok(Startables {
                                items,
                                next: Some(Cursor { phase, after: key }),
                                held,
                            });
                        }
                    }
                    match listing.next_after {
                        Some(after) => page.after = after,
                        None => break,
                    }
                }
            }
            Ok(Startables {
                items,
                next: None,
                held,
            })
        })
    }
}

/// The nodes of `listing`'s entries that a ready entry of the `earlier` bundled phases is
/// about.
fn touched(
    db: &Db,
    listing: &Listing<QueueEntry>,
    earlier: &[Phase],
    spent: &queues::Spent,
) -> Result<BTreeSet<NodeId>> {
    let nodes: BTreeSet<NodeId> = (listing.items.iter())
        .flat_map(|entry| entry.about.iter().copied())
        .collect();
    let mut touched = BTreeSet::new();
    if nodes.is_empty() {
        return Ok(touched);
    }
    let named: Vec<NodeId> = nodes.iter().copied().collect();
    for &phase in earlier {
        for entry in queues::naming(db, phase, &named, spent)? {
            touched.extend(
                entry
                    .about
                    .into_iter()
                    .filter(|about| nodes.contains(about)),
            );
        }
    }
    Ok(touched)
}

/// The other ready entries of `phase` sharing a node with `head` that nothing holds back, in
/// key order.
fn partners(
    db: &Db,
    phase: Phase,
    head: &QueueEntry,
    busy: &[Work],
    spent: &queues::Spent,
) -> Result<Vec<QueueEntry>> {
    Ok((queues::naming(db, phase, &head.about, spent)?.into_iter())
        .filter(|entry| {
            entry.node != head.node && !busy.iter().any(|work| work.holds(phase, entry))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{
        super::{
            queues::tests::queued,
            tests::{
                HIGH, assess, begun, component, fixture, operation, path_from, premise, scenario,
                serve, sweep, test, text, validated, verdict,
            },
        },
        *,
    };

    /// Ready work starts in pipeline order: settling looks at premises sharing a flaw go to one
    /// agent and hold back the review of the candidate resting on them, running work holds
    /// what shares a node with it, and decided premises release the review.
    #[test]
    fn startable_work_follows_the_pipeline_and_its_holds() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        sweep(&store, scenario.path);
        assess(&store, &actor, scenario.candidate, HIGH);
        let premises = [(); 2].map(|_| premise(&store, &actor, PremiseScope::Code));
        let links = premises.map(|premise| Link::FlawPremise {
            flaw: scenario.flaw,
            premise,
        });
        store.link(&actor, &links).unwrap();
        let [first, second] = premises.map(NodeId::from);
        let started = |running: &[NodeId]| {
            let page = store
                .startable(running, &[], &Live::default(), None, Bounded::new::<10>())
                .unwrap();
            (page.items.into_iter())
                .map(|item| {
                    let bundle = item.bundle.iter().map(|entry| entry.node).collect();
                    (item.entry.node, bundle)
                })
                .collect::<Vec<(NodeId, Vec<NodeId>)>>()
        };
        assert_eq!(started(&[]), [(first, vec![second])]);
        assert!(started(&[first]).is_empty());
        for premise in premises {
            let holds = StatusChange::Premise(premise, PremiseStatus::Holds);
            store.set_status(&actor, holds, &text("checked")).unwrap();
        }
        assert_eq!(started(&[]), [(scenario.candidate.into(), Vec::new())]);
    }

    /// A live task's subject merged away is still worked, as the node it was merged into: that
    /// node is not offered, and it holds back what shares a node with it.
    #[test]
    fn a_live_task_works_the_node_its_subject_was_merged_into() {
        let (_session, store, actor) = fixture();
        let (merged, kept) = (scenario(&store, &actor, "a"), scenario(&store, &actor, "b"));
        for path in [merged.path, kept.path] {
            sweep(&store, path);
        }
        let code = premise(&store, &actor, PremiseScope::Code);
        let link = Link::FlawPremise {
            flaw: kept.flaw,
            premise: code,
        };
        store.link(&actor, &[link]).unwrap();
        begun(&store, &Begin::Assess(vec![merged.candidate]));
        let merge = Merge::Candidate {
            candidate: merged.candidate,
            into: kept.candidate,
            note: text("same"),
        };
        store.merge(&actor, &merge).unwrap();
        assert!(queued(&store, Phase::Assess).is_empty());
        let page =
            (store.startable(&[], &[], &Live::default(), None, Bounded::new::<10>())).unwrap();
        assert_eq!(
            (page.items.iter())
                .map(|item| (item.phase, item.entry.node))
                .collect::<Vec<_>>(),
            [(Phase::Chain, merged.flaw.into())],
            "only the flaw the merge left unchained starts"
        );
    }

    /// Work the scheduler launched on a node merged away before its child began holds the node
    /// it was merged into.
    #[test]
    fn launched_work_holds_the_node_its_subject_was_merged_into() {
        let (_session, store, actor) = fixture();
        let (merged, kept) = (scenario(&store, &actor, "a"), scenario(&store, &actor, "b"));
        for path in [merged.path, kept.path] {
            sweep(&store, path);
        }
        let merge = Merge::Candidate {
            candidate: merged.candidate,
            into: kept.candidate,
            note: text("same"),
        };
        store.merge(&actor, &merge).unwrap();
        let running = [merged.candidate.into()];
        let page =
            (store.startable(&running, &[], &Live::default(), None, Bounded::new::<10>())).unwrap();
        assert!(
            !(page.items.iter()).any(|item| item.entry.node == kept.candidate.into()),
            "{:?}",
            page.items
        );
    }

    /// An unblock admits only the pending test it names: a resumption that records another
    /// pending test waits for an answer of its own. A pending test of a replaced verdict is not
    /// resumed: the new verdict owes a test of its own.
    #[test]
    fn an_unblock_admits_only_the_pending_test_it_answers() {
        let (_session, store, actor) = fixture();
        let scenario = validated(&store, &actor, "decoder");
        sweep(&store, scenario.path);
        let id = scenario.candidate;
        let first = test(&store, &actor, id, TestResult::Pending);
        let started = |unblocked: &[TestId]| {
            let tests = Live {
                approved: Vec::new(),
                unblocked: unblocked.to_vec(),
            };
            let page = (store.startable(&[], &[], &tests, None, Bounded::new::<10>())).unwrap();
            let entries = page
                .items
                .iter()
                .map(|item| (item.entry.node, item.entry.pending));
            (entries.collect::<Vec<_>>(), page.held)
        };
        assert_eq!(started(&[]), (Vec::new(), true));
        assert_eq!(started(&[first]), (vec![(id.into(), Some(first))], false));
        test(&store, &actor, id, TestResult::Pending);
        assert_eq!(
            started(&[first]),
            (Vec::new(), true),
            "a new pending test waits"
        );
        verdict(&store, &actor, id, Verdict::Validated);
        assert_eq!(started(&[]), (vec![(id.into(), None)], false));
    }

    /// A probe and a test the launch's policy admits start together, with nothing held.
    #[test]
    fn probes_and_admitted_tests_start_without_waiting() {
        let (_session, store, actor) = fixture();
        let scenario = validated(&store, &actor, "decoder");
        sweep(&store, scenario.path);
        let gateway = component(&store, &actor, "gateway");
        let probe = path_from(&store, &actor, "api", &[gateway], operation("gateway-api"));
        serve(&store, &actor, &["gateway-api"]).unwrap();
        let page =
            (store.startable(&[], &[], &Live::default(), None, Bounded::new::<10>())).unwrap();
        let items: Vec<_> = (page.items.iter())
            .map(|item| (item.phase, item.entry.node))
            .collect();
        assert_eq!(
            (items, page.held),
            (
                vec![
                    (Phase::Probe, probe.into()),
                    (Phase::Test, scenario.candidate.into())
                ],
                false
            )
        );
    }
}

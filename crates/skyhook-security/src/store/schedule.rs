//! What the orchestrator's scheduler starts next: the offered queue entries in pipeline
//! order, leaving out what is running, what running work holds back, later-stage work a ready
//! earlier-stage bundled entry holds back, and the tests it may not start yet, a page at a
//! time. A bundled entry comes with the ready entries of its bundle, which go to one agent.

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
    /// `live` admits. A bundled entry carries every other such entry of its bundle, and what one
    /// page starts holds back the rest of it as running work would.
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
            let mut owing = Vec::new();
            for &phase in &phases {
                if queues::owes(db, phase, spent)? {
                    owing.push(phase);
                }
            }
            let policy = runs::run(db)?.live;
            let mut busy = Work::running(db, &queues::live_subjects(db)?, running)?;
            let (mut items, mut held) = (Vec::new(), false);
            let passed = &phases[..first.unwrap_or(phases.len())];
            for &phase in owing.iter().filter(|phase| !passed.contains(phase)) {
                let earlier: Vec<Phase> = (owing.iter().copied())
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
                    let touched = touched(db, &listing.items, &earlier, spent)?;
                    for entry in listing.items {
                        let key = entry.key();
                        if waits(phase, &entry, &busy, &touched) {
                            continue;
                        }
                        if phase == Phase::Test && !live.admits(policy, &entry) {
                            held = true;
                            continue;
                        }
                        let bundle = match phase.bundled() {
                            true => partners(db, phase, &entry, &busy, &earlier, spent)?,
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

/// Whether `entry` of `phase` waits: running work holds it, or a ready earlier-stage bundled
/// entry is about a node it is about, one of `touched`.
fn waits(phase: Phase, entry: &QueueEntry, busy: &[Work], touched: &BTreeSet<NodeId>) -> bool {
    busy.iter().any(|work| work.holds(phase, entry))
        || entry.about.iter().any(|about| touched.contains(about))
}

/// The nodes of `entries` that a ready entry of the `earlier` bundled phases is about.
fn touched(
    db: &Db,
    entries: &[QueueEntry],
    earlier: &[Phase],
    spent: &queues::Spent,
) -> Result<BTreeSet<NodeId>> {
    let nodes: BTreeSet<NodeId> = (entries.iter())
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

/// The other ready entries of `head`'s bundle that nothing holds back, in key order.
fn partners(
    db: &Db,
    phase: Phase,
    head: &QueueEntry,
    busy: &[Work],
    earlier: &[Phase],
    spent: &queues::Spent,
) -> Result<Vec<QueueEntry>> {
    let bundle = queues::bundle(db, phase, head.node, spent)?;
    let touched = touched(db, &bundle, earlier, spent)?;
    Ok((bundle.into_iter())
        .filter(|entry| entry.node != head.node && !waits(phase, entry, busy, &touched))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{
        super::{
            queues::tests::queued,
            tests::{
                HIGH, access, assess, begun, candidate, check, component, decide, end_job, fixture,
                fixture_under, flaw, location, operation, path, path_from, premise, revalidate,
                scenario, serve, state, sweep, test, text, validated, verdict,
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
        assess(&store, scenario.candidate, HIGH);
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
            decide(&store, premise, PremiseStatus::Holds);
        }
        assert_eq!(started(&[]), [(scenario.candidate.into(), Vec::new())]);
    }

    /// The heads `startable` offers in `phase`, each with its bundle.
    fn bundles(store: &Store, phase: Phase) -> Vec<(NodeId, Vec<NodeId>)> {
        let page = store
            .startable(&[], &[], &Live::default(), None, Bounded::new::<10>())
            .unwrap();
        (page.items.into_iter())
            .filter(|item| item.phase == phase)
            .map(|item| {
                let bundle = item.bundle.iter().map(|entry| entry.node).collect();
                (item.entry.node, bundle)
            })
            .collect()
    }

    /// Flaws go to chaining and premises to settling with the rest of their first path's: a
    /// flaw on a stage two paths share goes with the lower path, and the other path's own work
    /// starts on its own. A premise's sub-claims, at any remove, go with it, by its first path.
    #[test]
    fn premises_and_flaws_bundle_by_their_first_path() {
        let (_session, store, actor) = fixture();
        let [a, b, c] = ["a", "b", "c"].map(|name| component(&store, &actor, name));
        let (first, second) = (
            path(&store, &actor, "p1", &[a, b]),
            path(&store, &actor, "p2", &[b, c]),
        );
        for path in [first, second] {
            sweep(&store, path);
        }
        let [on_a, shared, on_c] = [a, b, c].map(|component| flaw(&store, &actor, component));
        let [sub, relied, needed, later, deeper] =
            [(); 5].map(|_| premise(&store, &actor, PremiseScope::Code));
        let links = [
            Link::PathPremise {
                path: first,
                premise: relied,
            },
            Link::FlawPremise {
                flaw: on_a,
                premise: needed,
            },
            Link::FlawPremise {
                flaw: on_c,
                premise: later,
            },
            Link::PremiseBasis {
                premise: relied,
                rests_on: sub,
            },
            Link::PremiseBasis {
                premise: sub,
                rests_on: deeper,
            },
        ];
        store.link(&actor, &links).unwrap();
        assert_eq!(
            bundles(&store, Phase::Settle),
            [
                (
                    sub.into(),
                    vec![relied.into(), needed.into(), deeper.into()]
                ),
                (later.into(), Vec::new())
            ]
        );
        for premise in [sub, relied, needed, later, deeper] {
            decide(&store, premise, PremiseStatus::Holds);
        }
        assert_eq!(
            bundles(&store, Phase::Chain),
            [
                (on_a.into(), vec![shared.into()]),
                (on_c.into(), Vec::new())
            ]
        );
    }

    /// Paths go to sweeping with every other keyed on the state each keys on, the handed state
    /// it accesses that the most paths access, else such an instance state: a path accessing a
    /// lower instance state, or a lower handed state only it accesses, goes with the other
    /// readers of the shared handed state it reads, never joined through the instance state. A
    /// path accessing none goes alone. Checks are offered alone, once the sweeps are done.
    #[test]
    fn sweeps_bundle_by_their_key_state() {
        let (_session, store, actor) = fixture();
        let decoder = component(&store, &actor, "decoder");
        let [first, shared, second, apart] = ["first", "shared", "second", "apart"]
            .map(|name| path(&store, &actor, name, &[decoder]));
        let [window, token, carry, ring] = [
            NewState::Instance,
            NewState::Handed,
            NewState::Handed,
            NewState::Instance,
        ]
        .map(|scope| state(&store, &actor, scope));
        let at = location("decoder/lib.c", 9);
        let accesses = [
            access(first, window, Access::Write, at.clone()),
            access(shared, window, Access::Read, at.clone()),
            access(shared, token, Access::Read, at.clone()),
            access(shared, carry, Access::Read, at.clone()),
            access(second, carry, Access::Read, at.clone()),
            access(second, ring, Access::Write, at.clone()),
            access(shared, ring, Access::Read, at),
        ];
        store.link(&actor, &accesses).unwrap();
        let started = || {
            let page = store
                .startable(&[], &[], &Live::default(), None, Bounded::new::<10>())
                .unwrap();
            (page.items.into_iter())
                .map(|item| {
                    let bundle = item.bundle.iter().map(|entry| entry.node).collect();
                    (item.phase, item.entry.node, bundle)
                })
                .collect::<Vec<(Phase, NodeId, Vec<NodeId>)>>()
        };
        let [first, shared, second, apart] = [first, shared, second, apart].map(NodeId::from);
        let [window, token, carry, ring] = [window, token, carry, ring].map(NodeId::from);
        assert_eq!(
            started(),
            [
                (Phase::Sweep, first, vec![]),
                (Phase::Sweep, shared, vec![second]),
                (Phase::Sweep, apart, vec![]),
            ]
        );
        for path in [first, shared, second, apart] {
            sweep(&store, PathId(path.0));
        }
        assert_eq!(
            started(),
            [
                (Phase::Check, window, vec![]),
                (Phase::Check, token, vec![]),
                (Phase::Check, carry, vec![]),
                (Phase::Check, ring, vec![]),
            ]
        );
    }

    /// Candidates go to review with the rest of their root group, not with another root's
    /// candidate that only shares a flaw, which waits for the group it shares it with. A
    /// candidate whose flaw owes a ready chain waits for it as its group's partner too, from any
    /// page: its group's review starts without it.
    #[test]
    fn reviews_bundle_by_root_group() {
        let (_session, store, actor) = fixture();
        let (first, other) = (
            scenario(&store, &actor, "decoder"),
            scenario(&store, &actor, "parser"),
        );
        let next = flaw(&store, &actor, first.component);
        let variant = candidate(&store, &actor, first.path, &[first.flaw, next]);
        let crossing = candidate(&store, &actor, other.path, &[other.flaw, first.flaw]);
        let window = state(&store, &actor, NewState::Instance);
        let write = access(
            first.path,
            window,
            Access::Write,
            location("decoder/lib.c", 3),
        );
        store.link(&actor, &[write]).unwrap();
        for path in [first.path, other.path] {
            sweep(&store, path);
        }
        check(&store, window);
        for candidate in [first.candidate, other.candidate, variant, crossing] {
            assess(&store, candidate, HIGH);
        }
        assert_eq!(
            bundles(&store, Phase::Review),
            [
                (first.candidate.into(), vec![variant.into()]),
                (other.candidate.into(), Vec::new())
            ]
        );
        let wrong = Link::FlawState {
            flaw: next,
            state: window,
        };
        store.link(&actor, &[wrong]).unwrap();
        let started = |after| {
            let page = store
                .startable(&[], &[], &Live::default(), after, Bounded::new::<10>())
                .unwrap();
            (page.items.into_iter())
                .map(|item| {
                    let bundle = item.bundle.iter().map(|entry| entry.node).collect();
                    (item.phase, item.entry.node, bundle)
                })
                .collect::<Vec<(Phase, NodeId, Vec<NodeId>)>>()
        };
        let reviews = [first.candidate, other.candidate]
            .map(|candidate| (Phase::Review, candidate.into(), Vec::new()));
        let chain = (Phase::Chain, next.into(), Vec::new());
        assert_eq!(
            started(None),
            [chain, reviews[0].clone(), reviews[1].clone()]
        );
        let after = Cursor {
            phase: Phase::Review,
            after: 0,
        };
        assert_eq!(started(Some(after)), reviews);
    }

    /// Work on a subject merged away is worked as the node it was merged into: a live task's
    /// leaves that node unoffered and holds back what shares a node with it, and so does work
    /// the scheduler launched before its child began.
    #[test]
    fn running_work_holds_the_node_its_subject_was_merged_into() {
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
        let (_, job, _) = begun(&store, &Begin::Assess(vec![merged.candidate]));
        let merge = Merge {
            node: merged.candidate.into(),
            into: kept.candidate.into(),
            note: text("same"),
        };
        store.merge(&actor, &merge).unwrap();
        assert!(queued(&store, Phase::Assess).is_empty());
        let started = |running: &[NodeId]| {
            let page =
                (store.startable(running, &[], &Live::default(), None, Bounded::new::<10>()))
                    .unwrap();
            (page.items.iter())
                .map(|item| (item.phase, item.entry.node))
                .collect::<Vec<_>>()
        };
        let unchained = [(Phase::Chain, merged.flaw.into())];
        assert_eq!(
            started(&[]),
            unchained,
            "only the flaw the merge left unchained starts"
        );
        end_job(&store, job);
        assert_eq!(started(&[merged.candidate.into()]), unchained, "launched");
    }

    /// A test starts as the launch's live policy admits it. Under `ask` it waits, held, for the
    /// user's approval, and the store passes over the held tests to an approved one. An unblock
    /// admits only the pending test it names: a resumption that records another pending test
    /// waits for an answer of its own, and a pending test of a replaced verdict is not resumed:
    /// the new verdict owes a test of its own. A probe starts beside admitted tests.
    #[test]
    fn tests_start_as_the_live_policy_admits_them() {
        let (_session, store, actor) = fixture_under(LivePolicy::Ask);
        let scenario = validated(&store, &actor, "decoder");
        sweep(&store, scenario.path);
        let id = scenario.candidate;
        let started = |approved: &[CandidateId], unblocked: &[TestId], limit| {
            let live = Live {
                approved: approved.to_vec(),
                unblocked: unblocked.to_vec(),
            };
            let page = (store.startable(&[], &[], &live, None, limit)).unwrap();
            let entries =
                (page.items.iter()).map(|item| (item.phase, item.entry.node, item.entry.pending));
            (entries.collect::<Vec<_>>(), page.held)
        };
        let one = Bounded::new::<1>();
        assert_eq!(started(&[], &[], one), (Vec::new(), true));
        let first = test(&store, &actor, id, TestResult::Pending);
        assert_eq!(started(&[id], &[], one), (Vec::new(), true));
        assert_eq!(
            started(&[], &[first], one),
            (vec![(Phase::Test, id.into(), Some(first))], false)
        );
        test(&store, &actor, id, TestResult::Pending);
        assert_eq!(
            started(&[], &[first], one),
            (Vec::new(), true),
            "a new pending test waits"
        );
        revalidate(&store, id);
        let owed = (Phase::Test, id.into(), None);
        assert_eq!(started(&[id], &[], one), (vec![owed], false));

        let [held, approved] = ["parser", "lexer"].map(|name| validated(&store, &actor, name));
        for scenario in [held, approved] {
            sweep(&store, scenario.path);
        }
        let approved = approved.candidate;
        assert_eq!(
            started(&[approved], &[], one),
            (vec![(Phase::Test, approved.into(), None)], true)
        );

        let gateway = component(&store, &actor, "gateway");
        let probe = path_from(&store, &actor, "api", &[gateway], operation("gateway-api"));
        serve(&store, &actor, &["gateway-api"]).unwrap();
        assert_eq!(
            started(&[id, held.candidate, approved], &[], Bounded::new::<10>()),
            (
                vec![
                    (Phase::Probe, probe.into(), None),
                    owed,
                    (Phase::Test, held.candidate.into(), None),
                    (Phase::Test, approved.into(), None),
                ],
                false
            )
        );
    }

    /// A test spent on children that never began is exhausted for the run though no task of it
    /// exists, and holds back nothing: its root group's next test starts, beside another root's,
    /// and each root's report once its group's tests are spent.
    #[test]
    fn a_spent_test_releases_its_root_group() {
        let (_session, store, actor) = fixture();
        let first = validated(&store, &actor, "decoder");
        let sibling = candidate(&store, &actor, first.path, &[first.flaw]);
        assess(&store, sibling, HIGH);
        verdict(&store, sibling, Verdict::Validated);
        let other = validated(&store, &actor, "parser");
        for path in [first.path, other.path] {
            sweep(&store, path);
        }
        let started = |spent: &[SpentWork]| {
            let page = (store.startable(&[], spent, &Live::default(), None, Bounded::new::<10>()))
                .unwrap();
            assert!(!page.held);
            (page.items.into_iter())
                .map(|item| SpentWork {
                    phase: item.phase,
                    node: item.entry.node,
                    basis: item.entry.basis,
                })
                .collect::<Vec<_>>()
        };
        let nodes = |work: &[SpentWork]| -> Vec<(Phase, NodeId)> {
            work.iter().map(|work| (work.phase, work.node)).collect()
        };
        let [lead, beside] = started(&[])[..] else {
            panic!("each root group's lead test starts")
        };
        assert_eq!(
            nodes(&[lead, beside]),
            [first.candidate, other.candidate].map(|test| (Phase::Test, test.into()))
        );
        let [next, _] = started(&[lead])[..] else {
            panic!("the spent lead releases its group's next test")
        };
        assert_eq!(nodes(&[next]), [(Phase::Test, sibling.into())]);
        let spent = [lead, next, beside];
        assert_eq!(
            nodes(&started(&spent)),
            [first.flaw, other.flaw].map(|flaw| (Phase::Report, flaw.into()))
        );
        let exhausted = store.synthesis(&spent).unwrap().exhausted;
        let mut exhausted: Vec<_> = (exhausted.iter())
            .map(|entry| (entry.phase, entry.node, entry.tasks.len()))
            .collect();
        exhausted.sort();
        let mut expected: Vec<_> = (spent.iter())
            .map(|work| (work.phase, work.node, 0))
            .collect();
        expected.sort();
        assert_eq!(exhausted, expected);
    }
}

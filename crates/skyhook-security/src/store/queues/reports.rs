//! Report work: root flaws owing a report.

use super::*;

/// Candidate `$c` has finding work that is not exhausted: an assessment, a review or a test, or
/// a live test task recorded no test since it began.
macro_rules! finding_pending {
    ($c:expr) => {
        concat!(
            "(",
            pending!("assess", $c, assess_owed!($c)),
            " OR ",
            pending!("review", $c, review_owed!($c)),
            " OR ",
            pending!("test", $c, test_owed!($c)),
            " OR EXISTS (SELECT 1 FROM task_node ot JOIN task_state os ON os.task=ot.task WHERE \
             ot.phase='test' AND ",
            works!("ot.node", $c),
            " AND os.live AND NOT EXISTS (SELECT 1 FROM test ox WHERE ox.candidate=",
            $c,
            " AND ox.event>os.began)))"
        )
    };
}

/// Root `$f`'s finding reports its scenarios as they stand: none changed after what its last
/// write stands on. Only a report write publishes it; evidence on the finding leaves its prose
/// as old.
macro_rules! reported {
    ($f:expr) => {
        concat!(
            "ifnull((SELECT ",
            $crate::store::bearing::stamp!("report", "fd.reported", $f),
            " FROM finding fd WHERE fd.root=",
            $f,
            ")>=",
            $crate::store::bearing::basis!("report", $f),
            ",0)"
        )
    };
}
pub(in crate::store) use reported;

/// Finding `$f` is current, and published: unmerged, reporting its root's scenarios as they
/// stand, and with some to report.
macro_rules! current_finding {
    ($f:expr) => {
        concat!(
            "EXISTS (SELECT 1 FROM finding cu JOIN node cun ON cun.id=cu.node WHERE cu.node=",
            $f,
            " AND cun.merged_into IS NULL AND ",
            $crate::store::queues::reported!("cu.root"),
            " AND EXISTS (SELECT 1 FROM finding_scenario cus WHERE cus.root=cu.root))"
        )
    };
}
pub(in crate::store) use current_finding;

/// Owed while some scenario of the root's group is reportable and its finding does not report
/// them as they stand; ready once no member owes finding work that is not exhausted.
pub(super) const REPORT: Sql = phase_sql!(
    "report",
    "flaw",
    concat!(
        "EXISTS (SELECT 1 FROM finding_scenario WHERE root=n.id) AND NOT ",
        reported!("n.id")
    ),
    concat!(
        "NOT EXISTS (SELECT 1 FROM candidate_root rr JOIN node rn ON rn.id=rr.candidate WHERE \
         rr.root=n.id AND rn.merged_into IS NULL AND ",
        finding_pending!("rr.candidate"),
        ")"
    ),
    "NULL",
    "NULL"
);

#[cfg(test)]
mod tests {
    use super::{
        super::{
            super::tests::{
                HIGH, assess, begun, candidate, fixture, flaw, test, text, validated, verdict,
            },
            tests::{exhaust, queued},
        },
        *,
    };

    fn current(store: &Store, finding: FindingId) -> bool {
        let NodeView::Finding(view) = store.node(finding.into()).unwrap() else {
            panic!("a finding")
        };
        view.current
    }

    /// A root's report waits while a scenario of its group owes a test, and is owed again once
    /// what it reports changes: a new verdict, or a member joining the group. Only a report
    /// write makes it current again. Beginning work on
    /// a member changes nothing, so an exhausted report stays exhausted.
    #[test]
    fn a_report_follows_its_groups_scenarios() {
        let (_session, store, actor) = fixture();
        let scenario = validated(&store, &actor, "decoder");
        let (id, root) = (scenario.candidate, scenario.flaw);
        let owed = || queued(&store, Phase::Report) == [NodeId::from(root)];
        assert!(!owed(), "the test comes first");
        assert_eq!(store.synthesis(&[]).unwrap().owed(Phase::Report), 1);
        test(&store, &actor, id, TestResult::Confirmed);
        assert!(owed());
        let finding = store
            .report(&actor, root, &text("overflow"), &text("report"))
            .unwrap();
        let findings = || store.coverage().unwrap().findings;
        assert!(!owed() && current(&store, finding) && findings() == 1);
        verdict(&store, &actor, id, Verdict::Validated);
        assert!(
            !owed() && !current(&store, finding) && findings() == 0,
            "a new verdict makes it stale and waits on a test of its own"
        );
        let lone = flaw(&store, &actor, scenario.component);
        let unreported = store
            .report(&actor, lone, &text("nothing"), &text("report"))
            .unwrap();
        assert!(
            !current(&store, unreported),
            "a finding with no scenario is not current"
        );
        test(&store, &actor, id, TestResult::Confirmed);
        let disputed = NewEvidence {
            location: None,
            stance: Stance::Contradicts,
            note: text("the report misreads the bound"),
        };
        store.evidence(&actor, finding.into(), &[disputed]).unwrap();
        assert!(
            owed() && !current(&store, finding),
            "evidence on the finding publishes nothing"
        );
        store
            .report(&actor, root, &text("overflow"), &text("report"))
            .unwrap();

        let joined = candidate(&store, &actor, scenario.path, &[root]);
        for member in [id, joined] {
            assess(&store, &actor, member, HIGH);
            verdict(&store, &actor, member, Verdict::Validated);
        }
        test(&store, &actor, joined, TestResult::Confirmed);
        assert!(owed(), "a member joined");
        exhaust(&store, &Begin::Report(vec![root]));
        begun(&store, &Begin::Assess(vec![id]));
        assert!(!owed(), "a begin on a member changes nothing");

        let (reporter, _, _) = begun(&store, &Begin::Report(vec![root]));
        verdict(&store, &actor, id, Verdict::Validated);
        let finding = store
            .report(&reporter, root, &text("overflow"), &text("report"))
            .unwrap();
        assert!(
            !current(&store, finding),
            "a report stands on the basis its task began at"
        );
    }

    /// A root's report goes stale when a chain takes the root role from it, by a flaw put
    /// ahead of it, a reorder of the same flaws or a merge into another group, and stays
    /// current when its flaw joins another group at a later place.
    #[test]
    fn a_report_goes_stale_when_its_root_loses_the_role() {
        let (_session, store, actor) = fixture();
        let scenario = validated(&store, &actor, "decoder");
        let (first, second) = (scenario.flaw, flaw(&store, &actor, scenario.component));
        let report = |root| {
            store
                .report(&actor, root, &text("overflow"), &text("report"))
                .unwrap()
        };
        let chain = |flaws: Vec<FlawId>| {
            let edit = Edit {
                title: None,
                body: None,
                detail: CandidateEdit {
                    path: None,
                    chain: Some(flaws.try_into().unwrap()),
                },
            };
            let change = Upsert::Candidate(Change::Edit(scenario.candidate, edit));
            store.upsert(&actor, &change).unwrap();
        };
        let finding = report(first);
        candidate(&store, &actor, scenario.path, &[second, first]);
        assert!(
            current(&store, finding),
            "its flaw joining another group behind its root"
        );
        chain(vec![second, first]);
        assert!(!current(&store, finding), "a flaw put ahead takes the role");

        let finding = report(second);
        chain(vec![first, second]);
        assert!(!current(&store, finding), "a reorder moves the role");

        let (here, there) = (
            validated(&store, &actor, "lexer"),
            validated(&store, &actor, "parser"),
        );
        let sibling = candidate(&store, &actor, here.path, &[here.flaw]);
        assess(&store, &actor, sibling, HIGH);
        verdict(&store, &actor, sibling, Verdict::Validated);
        let finding = report(here.flaw);
        assert!(current(&store, finding));
        let merge = Merge::Candidate {
            candidate: here.candidate,
            into: there.candidate,
            note: text("same"),
        };
        store.merge(&actor, &merge).unwrap();
        assert!(
            !current(&store, finding),
            "a member merged into another group leaves it"
        );
    }
}

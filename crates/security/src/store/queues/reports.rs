//! Report work: root flaws owing a report.

use super::*;

/// Candidate `$c` has finding work that is not exhausted: a review or a test, or a live test
/// task recorded no test since it began.
macro_rules! finding_pending {
    ($c:expr) => {
        concat!(
            "(",
            pending!("review", $c, review_owed!($c)),
            " OR ",
            pending!("test", $c, test_owed!($c)),
            " OR EXISTS (SELECT 1 FROM live ot WHERE ot.phase='test' AND ot.node=",
            $c,
            " AND ",
            $crate::store::bearing::untested_since!($c, "ot.began"),
            "))"
        )
    };
}

/// Root `$f`'s finding reports its scenarios as they stand: its last write covered the
/// scenarios its group reports now, and none changed after what that write stands on, its task's
/// basis or, written outside one, its own event, but for tasks offered other scenarios. Only a
/// report write publishes it; evidence on the finding leaves its prose as old.
macro_rules! reported {
    ($f:expr) => {
        concat!(
            "ifnull((SELECT coalesce(",
            $crate::store::bearing::stamp!("report", "fd.reported", $f),
            ",fd.reported)>=",
            $crate::store::bearing::basis!(rested "report", $f),
            " AND ",
            $crate::store::bearing::reported_as!("fd.reported", "fd.root"),
            " FROM finding fd WHERE fd.root=",
            $f,
            "),0)"
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

/// Owed while some scenario of the root's group starting within the scope is reportable and its
/// finding does not report them as they stand; ready once no member owes finding work that is
/// not exhausted.
pub(super) const REPORT: Sql = phase_sql!(
    "report",
    "flaw",
    concat!(
        "EXISTS (SELECT 1 FROM finding_scenario fx WHERE fx.root=n.id AND NOT ",
        crate::store::scope::candidate_outside!("fx.candidate"),
        ") AND NOT ",
        reported!("n.id")
    ),
    concat!(
        "NOT EXISTS (SELECT 1 FROM candidate_root rr JOIN node rn ON rn.id=rr.candidate WHERE \
         rr.root=n.id AND rn.merged_into IS NULL AND ",
        finding_pending!("rr.candidate"),
        ")"
    ),
    "NULL",
    "NULL",
    "NULL"
);

#[cfg(test)]
mod tests {
    use super::{
        super::{
            super::tests::{
                HIGH, assess, begun, candidate, end_job, fixture, flaw, revalidate, reword, test,
                text, validated, verdict,
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
        assert_eq!(store.run_facts().unwrap().summary.owed(Phase::Report), 1);
        test(&store, &actor, id, TestResult::Confirmed);
        assert!(owed());
        let finding = store
            .report(&actor, root, &text("overflow"), &text("report"))
            .unwrap();
        let findings = || store.coverage().unwrap().findings;
        assert!(!owed() && current(&store, finding) && findings() == 1);
        revalidate(&store, id);
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
            environment: None,
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
            assess(&store, member, HIGH);
            verdict(&store, member, Verdict::Validated);
        }
        test(&store, &actor, joined, TestResult::Confirmed);
        assert!(owed(), "a member joined");
        exhaust(&store, &Begin::Report(vec![root]));
        begun(&store, &Begin::Review(vec![id]));
        assert!(!owed(), "a begin on a member changes nothing");

        let (reporter, _, _) = begun(&store, &Begin::Report(vec![root]));
        revalidate(&store, id);
        let finding = store
            .report(&reporter, root, &text("overflow"), &text("report"))
            .unwrap();
        assert!(
            !current(&store, finding),
            "a report stands on the basis its task began at"
        );
    }

    /// A report covers the scenarios its group reported as it was written: a member whose
    /// review restates its verdict, reportable again, owes the report a rewrite, with fresh
    /// tries though nothing its report rests on moved.
    #[test]
    fn a_report_is_owed_when_its_reportable_members_change() {
        let (_session, store, actor) = fixture();
        let reported = validated(&store, &actor, "decoder");
        let root = reported.flaw;
        test(&store, &actor, reported.candidate, TestResult::Confirmed);
        let member = candidate(&store, &actor, reported.path, &[root]);
        assess(&store, member, HIGH);
        verdict(&store, member, Verdict::Validated);
        reword(&store, &actor, member, "narrower than stated");
        for _ in 0..2 {
            let (_, job, _) = begun(&store, &Begin::Review(vec![member]));
            end_job(&store, job);
        }
        let offered = || queued(&store, Phase::Report) == [NodeId::from(root)];
        assert!(offered(), "the report waits on no exhausted review");
        let (_, job, _) = begun(&store, &Begin::Report(vec![root]));
        end_job(&store, job);
        let (reporter, job, _) = begun(&store, &Begin::Report(vec![root]));
        let finding = store
            .report(&reporter, root, &text("overflow"), &text("report"))
            .unwrap();
        end_job(&store, job);
        assert!(current(&store, finding), "it covers the one reportable");
        let proposed = text("proposed");
        (store.verdict(&actor, member, Verdict::Disproved, &proposed)).unwrap();
        verdict(&store, member, Verdict::Validated);
        assert!(!current(&store, finding));
        assert!(offered(), "a member reportable again gives it fresh tries");
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
                    actions: None,
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
        assess(&store, sibling, HIGH);
        verdict(&store, sibling, Verdict::Validated);
        let finding = report(here.flaw);
        assert!(current(&store, finding));
        let merge = Merge {
            node: here.candidate.into(),
            into: there.candidate.into(),
            note: text("same"),
        };
        store.merge(&actor, &merge).unwrap();
        assert!(
            !current(&store, finding),
            "a member merged into another group leaves it"
        );
    }
}

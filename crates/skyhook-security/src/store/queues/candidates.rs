//! Candidate work: candidates owing an assessment, a review or a test.

use super::*;

/// Candidate `$c` owes an assessment: none stands at what it rests on.
macro_rules! assess_owed {
    ($c:expr) => {
        concat!(
            "(SELECT NOT ak.assessed_current FROM candidate_core ak WHERE ak.candidate=",
            $c,
            ")"
        )
    };
}
pub(super) use assess_owed;

/// Candidate `$c` owes a review: assessed at or above the threshold, with no verdict standing.
macro_rules! review_owed {
    ($c:expr) => {
        concat!(
            "(SELECT vk.assessed_current AND vk.shortlisted AND NOT vk.reviewed_current FROM \
             candidate_core vk WHERE vk.candidate=",
            $c,
            ")"
        )
    };
}
pub(in crate::store) use review_owed;

/// Candidate `$c`'s latest test is pending at its latest verdict: it waits on the user or the
/// environment.
macro_rules! pending_test {
    ($c:expr) => {
        concat!(
            "ifnull((SELECT pt.result='pending' AND pt.verdict=pc.reviewed FROM test pt JOIN \
             candidate pc ON pc.node=pt.candidate WHERE pt.candidate=",
            $c,
            " ORDER BY pt.id DESC LIMIT 1),0)"
        )
    };
}
pub(super) use pending_test;

/// Candidate `$c`'s settling test settles its testing: a confirmed one while its chain is
/// uncontradicted, a declined one, and an unavailable or failed one until an environment its
/// tests rest on is recorded after it. A new verdict leaves any but a decline unsettled.
macro_rules! settled {
    ($c:expr) => {
        concat!(
            "ifnull((SELECT CASE st.result WHEN 'confirmed' THEN (SELECT NOT sk.contradicted FROM \
             candidate_core sk WHERE sk.candidate=st.candidate) WHEN 'declined' THEN 1 ELSE NOT \
             EXISTS (SELECT 1 FROM (",
            bearing!("test", "st.candidate"),
            ") sb JOIN event se ON se.id=sb.event WHERE sb.event>st.event AND \
             se.kind='environment') END FROM test st WHERE st.id=",
            $crate::store::bearing::settling_test!($c),
            "),0)"
        )
    };
}
pub(super) use settled;

/// The facts selecting tests, read once per statement for the root groups `$roots` selects,
/// every group without it: their [`demonstrations`], the candidates standing validated, which no
/// demonstration of their group notes, and of those the contenders, with no pending or settling
/// test and a test not exhausted, placed in their group by severity, then age.
macro_rules! tested {
    ($($roots:expr)?) => {
        concat!(
            $crate::store::bearing::demonstrations!($($roots)?),
            ",validated AS MATERIALIZED (SELECT upheld.candidate,upheld.root,upheld.rank FROM \
             upheld LEFT JOIN demonstration d ON d.root=upheld.root WHERE upheld.shortlisted AND \
             (upheld.demonstrated OR ifnull(d.rank<upheld.rank,1))),\
             contender AS MATERIALIZED (SELECT validated.*,row_number() OVER (PARTITION BY root \
             ORDER BY rank DESC,candidate) AS place FROM validated WHERE NOT ",
            pending_test!("validated.candidate"),
            " AND NOT ",
            settled!("validated.candidate"),
            " AND (",
            task_live!("test", "validated.candidate"),
            " OR ",
            retryable!("test", "validated.candidate"),
            "))"
        )
    };
}
pub(super) use tested;

/// Candidate `$c` stands validated and the launch tests validated candidates.
macro_rules! testable {
    ($c:expr) => {
        concat!(
            "((SELECT live FROM run)!='no' AND EXISTS (SELECT 1 FROM validated tv WHERE \
             tv.candidate=",
            $c,
            "))"
        )
    };
}
pub(super) use testable;

/// No candidate of `$c`'s root group comes before it for a test: its group's first contender is
/// none, `$c` itself, or less severe, or as severe and younger. A group's first demonstration goes to
/// its most severe scenario, and what it confirms notes the rest; a failed or exhausted one
/// holds nothing back.
macro_rules! leads {
    ($c:expr) => {
        concat!(
            "NOT EXISTS (SELECT 1 FROM validated lv JOIN contender ll ON ll.root=lv.root AND \
             ll.place=1 WHERE lv.candidate=",
            $c,
            " AND ll.candidate!=lv.candidate AND (ll.rank>lv.rank OR (ll.rank=lv.rank AND \
             ll.candidate<lv.candidate)))"
        )
    };
}
pub(super) use leads;

/// Candidate `$c` owes a test: it stands validated, and its latest test is pending, or none
/// settles it and it leads its group.
macro_rules! test_owed {
    ($c:expr) => {
        concat!(
            "(",
            testable!($c),
            " AND (",
            pending_test!($c),
            " OR (NOT ",
            settled!($c),
            " AND ",
            leads!($c),
            ")))"
        )
    };
}
pub(super) use test_owed;

/// The sweep or probe of no path candidate `$c` performs is pending.
macro_rules! structure_settled {
    ($c:expr) => {
        concat!(
            "NOT EXISTS (SELECT 1 FROM candidate_action ra WHERE ra.candidate=",
            $c,
            " AND ",
            structure_pending!("ra.path"),
            ")"
        )
    };
}

pub(super) const ASSESS: Sql = phase_sql!(
    "assess",
    "candidate",
    assess_owed!("n.id"),
    structure_settled!("n.id"),
    "NULL",
    "NULL"
);

/// Ready once no premise of its chain is undecided while the user is asked it or a settling
/// look at it is pending.
pub(super) const REVIEW: Sql = phase_sql!(
    "review",
    "candidate",
    review_owed!("n.id"),
    concat!(
        structure_settled!("n.id"),
        " AND NOT EXISTS (SELECT 1 FROM (",
        chain_premises!("n.id"),
        ") vp JOIN premise_state vq ON vq.node=vp.premise WHERE vq.status='undecided' AND \
         (vq.scope='deployment' OR ",
        settle_pending!("vq.node"),
        "))"
    ),
    "NULL",
    "NULL"
);

/// Ready once no settling look at a premise its chain rests on is pending. A pending test is
/// resumed as a test, naming it.
pub(super) const TEST: Sql = phase_sql!(
    "test",
    "candidate",
    test_owed!("n.id"),
    concat!(
        "NOT EXISTS (SELECT 1 FROM (",
        chain_premises!("n.id"),
        ") tp WHERE ",
        settle_pending!("tp.premise"),
        ")"
    ),
    concat!(
        "CASE WHEN ",
        pending_test!("n.id"),
        " THEN (SELECT max(id) FROM test WHERE candidate=n.id) END"
    ),
    "NULL"
);

#[cfg(test)]
mod tests {
    use skyhook::job::JobState;

    use super::{
        super::{
            super::tests::{
                HIGH, assess, begun, candidate, child_of, end_job, fixture, premise, scenario,
                sweep, test, text, validated, verdict,
            },
            tests::{exhaust, queued, status},
        },
        *,
    };

    const LOCAL: &str = "CVSS:4.0/AV:L/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N";

    /// An assessment and a review each stand until what they judge changes: a flaw's edit
    /// owes both, a premise decision only the review. A review waits on a pending settling look
    /// at its chain's premises, released once that look is exhausted, and on an undecided
    /// deployment premise, which the user is asked.
    #[test]
    fn assessments_and_reviews_follow_what_they_judge() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let id = scenario.candidate;
        let owed = |phase| queued(&store, phase) == [NodeId::from(id)];
        assert!(!owed(Phase::Assess), "its path's sweep comes first");
        sweep(&store, scenario.path);
        assert!(owed(Phase::Assess));
        assess(&store, &actor, id, HIGH);
        assert!(owed(Phase::Review));
        verdict(&store, &actor, id, Verdict::Validated);
        assert!(!owed(Phase::Review));

        let code = premise(&store, &actor, PremiseScope::Code);
        let rests = |premise| Link::FlawPremise {
            flaw: scenario.flaw,
            premise,
        };
        store.link(&actor, &[rests(code)]).unwrap();
        assert!(!owed(Phase::Review), "the settling look comes first");
        assert_eq!(store.run_facts().unwrap().summary.owed(Phase::Review), 1);
        exhaust(&store, &Begin::Settle(vec![code]));
        assert!(owed(Phase::Review), "an exhausted look holds nothing");
        verdict(&store, &actor, id, Verdict::Validated);
        let decided = StatusChange::Premise(code, PremiseStatus::Holds);
        store.set_status(&actor, decided, &text("checked")).unwrap();
        assert!(
            owed(Phase::Review) && !owed(Phase::Assess),
            "a decision owes review alone"
        );

        verdict(&store, &actor, id, Verdict::Validated);
        let edit = Edit {
            title: None,
            body: Some(text("the copy runs on every retry")),
            detail: FlawEdit::default(),
        };
        let edited = Upsert::Flaw(Change::Edit(scenario.flaw, edit));
        store.upsert(&actor, &edited).unwrap();
        assert!(owed(Phase::Assess) && !owed(Phase::Review));
        assess(&store, &actor, id, HIGH);
        assert!(owed(Phase::Review));

        let deployment = premise(&store, &actor, PremiseScope::Deployment);
        store.link(&actor, &[rests(deployment)]).unwrap();
        assert!(!owed(Phase::Review), "the user is asked first");
        let asked = store.synthesis(&[]).unwrap().asked;
        assert_eq!(
            asked.iter().map(|asked| asked.premise).collect::<Vec<_>>(),
            [deployment]
        );
        let answered = StatusChange::Premise(deployment, PremiseStatus::Holds);
        store
            .set_status(&actor, answered, &text("the user runs it"))
            .unwrap();
        assert!(owed(Phase::Review));
    }

    /// An assessment or a verdict recorded within a task stands on the basis the task began at:
    /// another agent's change after it began owes the work again.
    #[test]
    fn a_result_stands_on_the_basis_its_task_began_at() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let id = scenario.candidate;
        sweep(&store, scenario.path);
        let (assessor, job, _) = begun(&store, &Begin::Assess(vec![id]));
        let edit = Edit {
            title: None,
            body: Some(text("narrower than stated")),
            detail: CandidateEdit::default(),
        };
        let change = Upsert::Candidate(Change::Edit(id, edit));
        store.upsert(&actor, &change).unwrap();
        assess(&store, &assessor, id, HIGH);
        end_job(&store, job);
        assert_eq!(queued(&store, Phase::Assess), [NodeId::from(id)]);

        assess(&store, &actor, id, HIGH);
        let (reviewer, job, _) = begun(&store, &Begin::Review(vec![id]));
        let code = premise(&store, &actor, PremiseScope::Code);
        let holds = StatusChange::Premise(code, PremiseStatus::Holds);
        store.set_status(&actor, holds, &text("checked")).unwrap();
        let rests = Link::FlawPremise {
            flaw: scenario.flaw,
            premise: code,
        };
        store.link(&actor, &[rests]).unwrap();
        verdict(&store, &reviewer, id, Verdict::Validated);
        end_job(&store, job);
        assert_eq!(queued(&store, Phase::Review), [NodeId::from(id)]);
    }

    /// An unproven verdict stands only while a premise of its chain is undecided: one its own
    /// review decides leaves the review owed again.
    #[test]
    fn an_unproven_verdict_stands_while_a_premise_is_undecided() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let id = scenario.candidate;
        sweep(&store, scenario.path);
        assess(&store, &actor, id, HIGH);
        let code = premise(&store, &actor, PremiseScope::Code);
        let rests = Link::FlawPremise {
            flaw: scenario.flaw,
            premise: code,
        };
        store.link(&actor, &[rests]).unwrap();
        exhaust(&store, &Begin::Settle(vec![code]));
        let (reviewer, job, _) = begun(&store, &Begin::Review(vec![id]));
        verdict(&store, &reviewer, id, Verdict::Unproven);
        assert_eq!(status(&store, id), Status::Unproven);
        let decided = StatusChange::Premise(code, PremiseStatus::Holds);
        store
            .set_status(&reviewer, decided, &text("measured"))
            .unwrap();
        end_job(&store, job);
        assert_eq!(queued(&store, Phase::Review), [NodeId::from(id)]);
    }

    /// A root group is tested from its most severe scenario; once its test is exhausted the
    /// next one leads, and a demonstration notes the scenarios no more severe than it while
    /// it is its candidate's settling test and the positive verdict it tested stands.
    #[test]
    fn a_root_group_is_tested_from_its_most_severe_scenario() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let (severe, lesser) = (
            scenario.candidate,
            candidate(&store, &actor, scenario.path, &[scenario.flaw]),
        );
        sweep(&store, scenario.path);
        for (id, vector) in [(severe, HIGH), (lesser, LOCAL)] {
            assess(&store, &actor, id, vector);
            verdict(&store, &actor, id, Verdict::Validated);
        }
        assert_eq!(queued(&store, Phase::Test), [NodeId::from(severe)]);
        exhaust(&store, &Begin::Test(vec![severe]));
        assert_eq!(queued(&store, Phase::Test), [NodeId::from(lesser)]);
        test(&store, &actor, severe, TestResult::Confirmed);
        assert!(queued(&store, Phase::Test).is_empty());
        assert_eq!(status(&store, lesser), Status::Noted);
        assert_eq!(store.coverage().unwrap().candidates[&Status::Noted], 1);
        test(&store, &actor, severe, TestResult::Failed);
        assert_eq!(
            status(&store, lesser),
            Status::Validated,
            "a later settling test replaces the demonstration"
        );
        test(&store, &actor, severe, TestResult::Confirmed);
        verdict(&store, &actor, severe, Verdict::Validated);
        assert_eq!(
            (status(&store, lesser), queued(&store, Phase::Test)),
            (Status::Validated, vec![NodeId::from(severe)]),
            "a re-validation needs a demonstration of its own"
        );
        verdict(&store, &actor, severe, Verdict::Disproved);
        assert_eq!(
            status(&store, lesser),
            Status::Validated,
            "a demonstration whose verdict no longer stands notes nothing"
        );
    }

    /// A settled test is owed again only once what it rests on changed after it: a declined
    /// one never, a new verdict included, an unavailable or failed one after an environment,
    /// and any other once a new verdict is recorded. An environment a test's own work records
    /// resets nothing, and a test whose verdict was replaced while it ran settles nothing.
    #[test]
    fn a_settled_test_is_owed_again_only_after_what_it_rests_on_changed() {
        let (_session, store, actor) = fixture();
        let scenario = validated(&store, &actor, "decoder");
        let id = scenario.candidate;
        let owed = || queued(&store, Phase::Test) == [NodeId::from(id)];
        let environment = |actor: &AgentId| {
            let input = NewEnvironment {
                instructions: text("start the container"),
                candidate: None,
                serves: Serves::default(),
            };
            store.environment(actor, &input).unwrap();
        };
        assert!(owed());
        test(&store, &actor, id, TestResult::Declined);
        environment(&actor);
        verdict(&store, &actor, id, Verdict::Validated);
        assert!(!owed(), "a decline stands across verdicts");
        test(&store, &actor, id, TestResult::Unavailable);
        assert!(!owed());
        environment(&actor);
        assert!(owed());
        test(&store, &actor, id, TestResult::Failed);
        assert!(!owed());
        verdict(&store, &actor, id, Verdict::Validated);
        assert!(owed(), "a new verdict needs a test of its own");

        let (prover, job, _) = begun(&store, &Begin::Test(vec![id]));
        test(&store, &prover, id, TestResult::Unavailable);
        let (preparer, _) = child_of(&store, &prover, JobState::Running);
        environment(&preparer);
        end_job(&store, job);
        assert!(!owed(), "its own environment");
        environment(&actor);
        assert!(owed());

        test(&store, &actor, id, TestResult::Confirmed);
        assert!(!owed());
        verdict(&store, &actor, id, Verdict::Validated);
        assert!(owed(), "a re-validation owes its demonstration again");

        let (prover, job, _) = begun(&store, &Begin::Test(vec![id]));
        verdict(&store, &actor, id, Verdict::Validated);
        test(&store, &prover, id, TestResult::Confirmed);
        end_job(&store, job);
        assert!(owed(), "it tested the verdict its task began at");
    }

    /// Work owed with nothing outside its own tasks to rest on, as a test of the candidate a
    /// tester restated its subject as, rests on the subject's latest event.
    #[test]
    fn owed_work_always_has_a_basis() {
        let (_session, store, actor) = fixture();
        let scenario = validated(&store, &actor, "decoder");
        sweep(&store, scenario.path);
        let (prover, job, _) = begun(&store, &Begin::Test(vec![scenario.candidate]));
        let restated = candidate(&store, &prover, scenario.path, &[scenario.flaw]);
        let merge = Merge::Candidate {
            candidate: scenario.candidate,
            into: restated,
            note: text("restated"),
        };
        store.merge(&prover, &merge).unwrap();
        assess(&store, &prover, restated, HIGH);
        verdict(&store, &prover, restated, Verdict::Validated);
        end_job(&store, job);
        assert_eq!(queued(&store, Phase::Test), [NodeId::from(restated)]);
    }
}

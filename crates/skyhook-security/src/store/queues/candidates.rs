//! Candidate work: candidates owing an assessment, a review or a test.

use super::*;

/// Candidate `$c` owes an assessment: it starts within the scope, and none stands at what it
/// rests on, or one recorded since outside its work proposes another.
macro_rules! assess_owed {
    ($c:expr) => {
        concat!(
            "(NOT ",
            $crate::store::scope::candidate_outside!($c),
            " AND (SELECT NOT ak.assessed_current OR ak.assessment_proposed IS NOT NULL FROM \
             candidate_core ak WHERE ak.candidate=",
            $c,
            "))"
        )
    };
}
pub(super) use assess_owed;

/// Candidate `$c` owes a review: it starts within the scope, assessed at or above the
/// threshold, with no verdict standing, or one recorded since outside its work proposing
/// another.
macro_rules! review_owed {
    ($c:expr) => {
        concat!(
            "(NOT ",
            $crate::store::scope::candidate_outside!($c),
            " AND (SELECT vk.assessed_current AND vk.shortlisted AND (NOT vk.reviewed_current OR \
             vk.verdict_proposed IS NOT NULL) FROM candidate_core vk WHERE vk.candidate=",
            $c,
            "))"
        )
    };
}
pub(in crate::store) use review_owed;

/// Candidate `$c`'s latest test is pending with what it demonstrates unchanged: it waits on the
/// user or the environment.
macro_rules! pending_test {
    ($c:expr) => {
        concat!(
            "ifnull((SELECT pt.result='pending' AND ",
            $crate::store::bearing::tested_unchanged!($c, "pt.event"),
            " FROM test pt WHERE pt.candidate=",
            $c,
            " ORDER BY pt.id DESC LIMIT 1),0)"
        )
    };
}
pub(super) use pending_test;

/// Candidate `$c`'s settling test settles its testing: a confirmed one while its chain is
/// uncontradicted, a declined one, and an unavailable or failed one until an environment its
/// tests rest on is recorded after it. A verdict of another value or a change of its chain or
/// actions leaves any but a decline unsettled.
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
/// every group without it: their [`demonstrations`], the candidates standing validated that
/// start within the scope, which no demonstration of their group notes, and of those the
/// contenders, with no pending or settling
/// test and a test not exhausted, placed in their group by severity, then age.
macro_rules! tested {
    ($($roots:expr)?) => {
        concat!(
            $crate::store::bearing::demonstrations!($($roots)?),
            ",validated AS MATERIALIZED (SELECT upheld.candidate,upheld.root,upheld.rank FROM \
             upheld LEFT JOIN demonstration d ON d.root=upheld.root WHERE upheld.shortlisted AND \
             (upheld.demonstrated OR ifnull(d.rank<upheld.rank,1)) AND NOT ",
            $crate::store::scope::candidate_outside!("upheld.candidate"),
            "),contender AS MATERIALIZED (SELECT validated.*,row_number() OVER (PARTITION BY root \
             ORDER BY rank DESC,candidate) AS place FROM validated WHERE NOT ",
            pending_test!("validated.candidate"),
            " AND NOT ",
            settled!("validated.candidate"),
            " AND ",
            unexhausted!("test", "validated.candidate"),
            ")"
        )
    };
}
pub(super) use tested;

/// Row `$k` of `candidate_core` stands unproven: live, shortlisted and starting within the
/// scope, its assessment and verdict standing.
macro_rules! unproven {
    ($k:literal) => {
        concat!(
            "NOT ",
            $crate::store::scope::candidate_outside!(concat!($k, ".candidate")),
            " AND ",
            $k,
            ".merged_into IS NULL AND ",
            $k,
            ".verdict='unproven' AND ",
            $k,
            ".assessed_current AND ",
            $k,
            ".reviewed_current AND ",
            $k,
            ".shortlisted"
        )
    };
}
pub(super) use unproven;

/// The launch does live work and candidate `$c` stands validated, or unproven, starting within
/// the scope.
macro_rules! testable {
    ($c:expr) => {
        concat!(
            "((SELECT live FROM run)!='no' AND (EXISTS (SELECT 1 FROM validated tv WHERE \
             tv.candidate=",
            $c,
            ") OR EXISTS (SELECT 1 FROM candidate_core tu WHERE tu.candidate=",
            $c,
            " AND ",
            unproven!("tu"),
            ")))"
        )
    };
}
pub(super) use testable;

/// Candidate `$c` stands validated, or it leads the test of a premise blocking it: an undecided
/// code premise of its chain whose settle is exhausted, which no other unproven candidate resting
/// on it comes before it for, more severe, or as severe and older, with a test not exhausted.
/// What reading could not settle is tried by one test, and the rest wait for what it decides.
macro_rules! premise_leads {
    ($c:expr) => {
        concat!(
            "(EXISTS (SELECT 1 FROM validated pv WHERE pv.candidate=",
            $c,
            ") OR EXISTS (SELECT 1 FROM (",
            chain_premises!($c),
            ") cp JOIN premise_standing bq ON bq.node=cp.premise WHERE bq.status='undecided' AND ",
            settle_owed!("bq.node"),
            " AND NOT ",
            unexhausted!("settle", "bq.node"),
            " AND NOT EXISTS (SELECT 1 FROM candidate_core bo WHERE bo.candidate IN (",
            $crate::store::bearing::premise_candidates!("bq.node"),
            ") AND bo.candidate!=",
            $c,
            " AND ",
            unproven!("bo"),
            " AND (bo.rank,-bo.candidate)>(SELECT bk.rank,-bc.candidate FROM candidate_result bc \
             JOIN severity_kind bk ON bk.name=bc.severity WHERE bc.candidate=",
            $c,
            ") AND ",
            unexhausted!("test", "bo.candidate"),
            ")))"
        )
    };
}
pub(super) use premise_leads;

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

/// Candidate `$c` owes a test: the launch tests it, and its latest test is pending, or none
/// settles it and it leads its group and a premise blocking it.
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
            " AND ",
            premise_leads!($c),
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
    "NULL",
    "NULL"
);

/// Ready once its assessment is not pending, and no premise of its chain is undecided while the
/// user is asked it or a settling look at it is pending. Bundled by root group: a candidate goes
/// with every ready one sharing its root flaw.
pub(super) const REVIEW: Sql = phase_sql!(
    "review",
    "candidate",
    review_owed!("n.id"),
    concat!(
        structure_settled!("n.id"),
        " AND NOT ",
        pending!("assess", "n.id", assess_owed!("n.id")),
        " AND NOT EXISTS (SELECT 1 FROM (",
        chain_premises!("n.id"),
        ") vp JOIN premise_standing vq ON vq.node=vp.premise WHERE vq.status='undecided' AND \
         (vq.scope='deployment' OR ",
        settle_pending!("vq.node"),
        "))"
    ),
    "NULL",
    concat!(
        "SELECT value FROM json_each(?1) UNION SELECT candidate FROM candidate_flaw WHERE flaw IN \
         (SELECT value FROM json_each(?1)) UNION SELECT cf.candidate FROM flaw_premise fp JOIN \
         candidate_flaw cf ON cf.flaw=fp.flaw WHERE fp.premise IN (SELECT value FROM \
         json_each(?1)) UNION SELECT pa.candidate FROM ",
        action_premises!("pp.premise IN (SELECT value FROM json_each(?1))")
    ),
    "SELECT value FROM json_each(?1) UNION SELECT r.candidate FROM candidate_root h JOIN \
     candidate_root r ON r.root=h.root WHERE h.candidate IN (SELECT value FROM json_each(?1))"
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
    "NULL",
    "NULL"
);

/// The blocking premise of candidate `$c` that a more severe unproven candidate is being tested
/// for, as `bq.node`, with that candidate as `bo.candidate`, while no test of `$c` is pending or
/// settles it: what [`premise_leads`] waits on.
macro_rules! premise_lead {
    ($c:expr, $select:literal) => {
        concat!(
            "(SELECT ",
            $select,
            " FROM (",
            chain_premises!($c),
            ") cp JOIN premise_standing bq ON bq.node=cp.premise JOIN candidate_core bo ON \
             bo.candidate IN (",
            $crate::store::bearing::premise_candidates!("bq.node"),
            ") WHERE NOT ",
            pending_test!($c),
            " AND NOT ",
            settled!($c),
            " AND bq.status='undecided' AND ",
            settle_owed!("bq.node"),
            " AND NOT ",
            unexhausted!("settle", "bq.node"),
            " AND bo.candidate!=",
            $c,
            " AND ",
            unproven!("bo"),
            " AND (bo.rank,-bo.candidate)>(SELECT bk.rank,-bc.candidate FROM candidate_result bc \
             JOIN severity_kind bk ON bk.name=bc.severity WHERE bc.candidate=",
            $c,
            ") AND ",
            unexhausted!("test", "bo.candidate"),
            " ORDER BY bo.rank DESC,bo.candidate LIMIT 1)"
        )
    };
}

/// For each of the candidates bound as `?1`: the demonstrated member of its root group that
/// notes it, the member its group's test goes to first while it contends for one ([`leads`]),
/// and the more severe candidate tested for one of its blocking premises with that premise
/// while no test of its own is pending or settles it ([`premise_leads`]).
const BEHIND: &str = concat!(
    relations!("SELECT value FROM json_each(?1)"),
    "SELECT j.value,(SELECT o.candidate FROM candidate_state s JOIN candidate_root o ON \
     o.root=s.root JOIN candidate_core d ON d.candidate=o.candidate WHERE s.candidate=j.value AND \
     s.state='noted' AND o.candidate!=s.candidate AND d.merged_into IS NULL AND d.upheld AND \
     d.demonstrated AND d.rank>=s.rank ORDER BY d.rank DESC,o.candidate LIMIT 1),(SELECT \
     ll.candidate FROM contender lv JOIN contender ll ON ll.root=lv.root AND ll.place=1 WHERE \
     lv.candidate=j.value AND lv.place>1),",
    premise_lead!("j.value", "bo.candidate"),
    ",",
    premise_lead!("j.value", "bq.node"),
    " FROM json_each(?1) j"
);

/// Which other candidate each of `candidates` waits behind, and why; none for the rest.
pub(in crate::store) fn behind(db: &Db, candidates: &[NodeId]) -> Result<Vec<(NodeId, Behind)>> {
    let found = rows(
        db,
        BEHIND,
        Spent::default().bound(params![json_list(candidates)?]),
        |row| {
            let lead = |index| -> Result<Option<CandidateId>> {
                Ok(row.get::<Option<i64>>(index)?.map(CandidateId))
            };
            let behind = match (lead(1)?, lead(2)?, lead(3)?) {
                (Some(lead), _, _) => Some(Behind::Noted { lead }),
                (None, Some(lead), _) => Some(Behind::Test { lead }),
                (None, None, Some(lead)) => Some(Behind::PremiseTest {
                    lead,
                    premise: PremiseId(row.get(4)?),
                }),
                (None, None, None) => None,
            };
            Ok((NodeId(row.get(0)?), behind))
        },
    )?;
    Ok(found
        .into_iter()
        .filter_map(|(node, behind)| behind.map(|behind| (node, behind)))
        .collect())
}

/// Why a candidate waits on another member of its root group or of its blocking premise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Behind {
    /// A demonstrated scenario at its severity or above reports its root.
    Noted { lead: CandidateId },
    /// Its group's test goes to `lead` first.
    Test { lead: CandidateId },
    /// `lead` is tested for `premise`, which blocks it too.
    PremiseTest {
        lead: CandidateId,
        premise: PremiseId,
    },
}

#[cfg(test)]
mod tests {
    use skyhook::job::JobState;

    use super::{
        super::{
            super::tests::{
                HIGH, access, assess, begun, candidate, child_of, decide, end_job, fixture, judge,
                location, path, premise, revalidate, scenario, state, sweep, test, text, validated,
                verdict, within,
            },
            tests::{exhaust, queued, status},
        },
        *,
    };

    const LOCAL: &str = "CVSS:4.0/AV:L/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N";

    /// An assessment and a review each stand until what they judge changes: a flaw's edit
    /// owes both, a premise linked to its flaw or decided only the review, and another path's
    /// access of a state its path shares neither. A review waits on a pending settling look at
    /// its chain's premises, released once that look is exhausted, and on an undecided
    /// deployment premise, which the user is asked. Evidence a review records owes the
    /// assessment again, and an assessment restating the standing one is current and owes no
    /// review.
    #[test]
    fn assessments_and_reviews_follow_what_they_judge() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let id = scenario.candidate;
        let window = state(&store, &actor, NewState::Instance);
        let at = location("decoder/lib.c", 3);
        let write = access(scenario.path, window, Access::Write, at.clone());
        store.link(&actor, &[write]).unwrap();
        let owed = |phase| queued(&store, phase) == [NodeId::from(id)];
        assert!(!owed(Phase::Assess), "its path's sweep comes first");
        sweep(&store, scenario.path);
        assert!(owed(Phase::Assess));
        assess(&store, id, HIGH);
        assert!(owed(Phase::Review));
        verdict(&store, id, Verdict::Validated);
        assert!(!owed(Phase::Review));
        let other = path(&store, &actor, "consumer", &[scenario.component]);
        let read = access(other, window, Access::Read, at);
        store.link(&actor, &[read]).unwrap();
        let summary = store.run_facts().unwrap().summary;
        assert_eq!(
            (summary.owed(Phase::Assess), summary.owed(Phase::Review)),
            (0, 0),
            "another path's access of a state it shares"
        );

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
        verdict(&store, id, Verdict::Validated);
        decide(&store, code, PremiseStatus::Holds);
        assert!(
            owed(Phase::Review) && !owed(Phase::Assess),
            "a decision owes review alone"
        );

        verdict(&store, id, Verdict::Validated);
        let edit = Edit {
            title: None,
            body: Some(text("the copy runs on every retry")),
            detail: FlawEdit::default(),
        };
        let edited = Upsert::Flaw(Change::Edit(scenario.flaw, edit));
        store.upsert(&actor, &edited).unwrap();
        assert!(owed(Phase::Assess) && !owed(Phase::Review));
        assess(&store, id, HIGH);
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

        judge(&store, scenario.flaw, FlawStatus::Refuted);
        assess(&store, id, HIGH);
        within(&store, &Begin::Review(vec![id]), |reviewer| {
            store
                .verdict(reviewer, id, Verdict::Disproved, &text("reviewed"))
                .unwrap();
            let contradicts = NewEvidence {
                location: None,
                environment: None,
                stance: Stance::Contradicts,
                note: text("the copy is bounded"),
            };
            store.evidence(reviewer, id.into(), &[contradicts]).unwrap();
        });
        assert!(owed(Phase::Assess) && !owed(Phase::Review));
        assess(&store, id, HIGH);
        assert_eq!(
            (owed(Phase::Review), status(&store, id)),
            (false, Status::Disproved),
            "a restated assessment is current and owes no review"
        );
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
        let vector = HIGH.parse().unwrap();
        store
            .assess(&assessor, id, vector, &text("scored"))
            .unwrap();
        end_job(&store, job);
        assert_eq!(queued(&store, Phase::Assess), [NodeId::from(id)]);

        assess(&store, id, HIGH);
        let (reviewer, job, _) = begun(&store, &Begin::Review(vec![id]));
        let code = premise(&store, &actor, PremiseScope::Code);
        decide(&store, code, PremiseStatus::Holds);
        let rests = Link::FlawPremise {
            flaw: scenario.flaw,
            premise: code,
        };
        store.link(&actor, &[rests]).unwrap();
        store
            .verdict(&reviewer, id, Verdict::Validated, &text("reviewed"))
            .unwrap();
        end_job(&store, job);
        assert_eq!(queued(&store, Phase::Review), [NodeId::from(id)]);
    }

    /// An unproven verdict stands only while a premise of its chain is undecided: its own
    /// review's decision only proposes one, and the review is owed again once its settle
    /// decides it.
    #[test]
    fn an_unproven_verdict_stands_while_a_premise_is_undecided() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let id = scenario.candidate;
        sweep(&store, scenario.path);
        assess(&store, id, HIGH);
        let code = premise(&store, &actor, PremiseScope::Code);
        let rests = Link::FlawPremise {
            flaw: scenario.flaw,
            premise: code,
        };
        store.link(&actor, &[rests]).unwrap();
        exhaust(&store, &Begin::Settle(vec![code]));
        let (reviewer, job, _) = begun(&store, &Begin::Review(vec![id]));
        store
            .verdict(&reviewer, id, Verdict::Unproven, &text("reviewed"))
            .unwrap();
        assert_eq!(status(&store, id), Status::Unproven);
        let decided = StatusChange::Premise(code, PremiseStatus::Holds);
        store
            .set_status(&reviewer, decided, &text("measured"))
            .unwrap();
        end_job(&store, job);
        assert_eq!(status(&store, id), Status::Unproven);
        assert_eq!(queued(&store, Phase::Settle), [NodeId::from(code)]);
        decide(&store, code, PremiseStatus::Holds);
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
            assess(&store, id, vector);
            verdict(&store, id, Verdict::Validated);
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
        verdict(&store, severe, Verdict::Disproved);
        assert_eq!(
            status(&store, lesser),
            Status::Validated,
            "a demonstration whose verdict no longer stands notes nothing"
        );
    }

    /// An unproven candidate is tested once its chain rests on an undecided code premise whose
    /// settle is exhausted, not on one still being settled or one only the user can answer;
    /// through the most severe unproven candidate resting on it, the next once that one's test
    /// is exhausted. A test observing nothing settles it, holding the others back, until what it
    /// rests on changes; one observing the premise proposes it to its settle, whose decision owes
    /// their reviews, and a validation then owes its group's demonstration.
    #[test]
    fn an_unproven_candidate_is_tested_for_a_premise_reading_left_undecided() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let (lesser, severe) = (
            scenario.candidate,
            candidate(&store, &actor, scenario.path, &[scenario.flaw]),
        );
        sweep(&store, scenario.path);
        assess(&store, lesser, LOCAL);
        assess(&store, severe, HIGH);
        let rests = |premise| Link::FlawPremise {
            flaw: scenario.flaw,
            premise,
        };
        let unproven = || {
            for id in [lesser, severe] {
                verdict(&store, id, Verdict::Unproven);
            }
        };
        let tested = || queued(&store, Phase::Test);
        let owed = || store.run_facts().unwrap().summary.owed(Phase::Test);
        let deployment = premise(&store, &actor, PremiseScope::Deployment);
        store.link(&actor, &[rests(deployment)]).unwrap();
        unproven();
        assert_eq!(owed(), 0, "the user is asked");
        let answered = StatusChange::Premise(deployment, PremiseStatus::Holds);
        store
            .set_status(&actor, answered, &text("the user runs it"))
            .unwrap();
        let code = premise(&store, &actor, PremiseScope::Code);
        store.link(&actor, &[rests(code)]).unwrap();
        unproven();
        assert_eq!(owed(), 0, "its settle comes first");
        exhaust(&store, &Begin::Settle(vec![code]));
        assert_eq!(tested(), [NodeId::from(severe)]);
        exhaust(&store, &Begin::Test(vec![severe]));
        assert_eq!(tested(), [NodeId::from(lesser)]);

        let (prover, job, _) = begun(&store, &Begin::Test(vec![lesser]));
        let tried = NewEvidence {
            location: None,
            environment: None,
            stance: Stance::Context,
            note: text("the bounded plan showed nothing"),
        };
        store.evidence(&prover, code.into(), &[tried]).unwrap();
        test(&store, &prover, lesser, TestResult::Unavailable);
        end_job(&store, job);
        assert!(tested().is_empty() && queued(&store, Phase::Review).is_empty());
        let environment = NewEnvironment {
            instructions: text("start the container"),
            candidate: None,
            serves: Serves::default(),
        };
        store.environment(&actor, &environment).unwrap();
        assert_eq!(tested(), [NodeId::from(severe)]);

        let (prover, job, _) = begun(&store, &Begin::Test(vec![severe]));
        let observed = StatusChange::Premise(code, PremiseStatus::Holds);
        store
            .set_status(&prover, observed, &text("the sanitizer run"))
            .unwrap();
        test(&store, &prover, severe, TestResult::Confirmed);
        end_job(&store, job);
        assert_eq!(queued(&store, Phase::Settle), [NodeId::from(code)]);
        decide(&store, code, PremiseStatus::Holds);
        assert_eq!(
            queued(&store, Phase::Review),
            [NodeId::from(lesser), severe.into()]
        );
        for id in [lesser, severe] {
            verdict(&store, id, Verdict::Validated);
        }
        assert_eq!(
            (tested(), status(&store, lesser)),
            (vec![NodeId::from(severe)], Status::Validated),
            "a test at an unproven verdict demonstrates nothing"
        );
    }

    /// A settled test is owed again only once what it rests on changed after it: a declined
    /// one never, a new verdict included, an unavailable or failed one after an environment,
    /// and any other once a verdict of another value is recorded or its chain changes, not when
    /// a verdict is restated after evidence alone. An environment a test's own work records
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
        revalidate(&store, id);
        assert!(!owed(), "a decline stands across verdicts");
        test(&store, &actor, id, TestResult::Unavailable);
        assert!(!owed());
        environment(&actor);
        assert!(owed());
        test(&store, &actor, id, TestResult::Failed);
        verdict(&store, id, Verdict::Validated);
        assert!(!owed(), "a restated verdict leaves it standing");
        revalidate(&store, id);
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
        let contradicts = NewEvidence {
            location: None,
            environment: None,
            stance: Stance::Contradicts,
            note: text("the copy is bounded"),
        };
        store.evidence(&actor, id.into(), &[contradicts]).unwrap();
        assess(&store, id, HIGH);
        verdict(&store, id, Verdict::Validated);
        assert!(!owed(), "a restatement after evidence alone keeps it");
        let code = premise(&store, &actor, PremiseScope::Code);
        let rests = Link::FlawPremise {
            flaw: scenario.flaw,
            premise: code,
        };
        store.link(&actor, &[rests]).unwrap();
        decide(&store, code, PremiseStatus::Holds);
        verdict(&store, id, Verdict::Validated);
        assert!(
            owed(),
            "a re-validation after its chain changed owes a test"
        );

        test(&store, &actor, id, TestResult::Confirmed);
        let (prover, job, _) = begun(&store, &Begin::Test(vec![id]));
        revalidate(&store, id);
        test(&store, &prover, id, TestResult::Confirmed);
        end_job(&store, job);
        assert!(owed(), "it tested the verdict its task began at");
    }
}

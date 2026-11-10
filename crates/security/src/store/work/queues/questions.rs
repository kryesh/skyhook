//! Question work: premises owing a settling look and flaws owing a chain.

use super::*;

/// Candidate `$c` is live: unmerged, and not judged disproved or excluded at its current review.
/// A flaw only such candidates hold is chained again, for a route resting on other premises.
macro_rules! live_candidate {
    ($c:expr) => {
        concat!(
            "EXISTS (SELECT 1 FROM candidate_core lc WHERE lc.candidate=",
            $c,
            " AND lc.merged_into IS NULL AND NOT (lc.reviewed_current AND \
             ifnull(lc.verdict IN ('disproved','excluded'),0)))"
        )
    };
}
pub(super) use live_candidate;

/// Flaw `$f` is live: unmerged and suspected, on no unmerged candidate or on a live one.
macro_rules! live_flaw {
    ($f:expr) => {
        concat!(
            "(EXISTS (SELECT 1 FROM flaw_standing lf JOIN node lfn ON lfn.id=lf.node WHERE \
             lf.node=",
            $f,
            " AND lfn.merged_into IS NULL AND lf.status='suspected') AND (NOT EXISTS (SELECT 1 \
             FROM candidate_flaw lfa JOIN node lfm ON lfm.id=lfa.candidate WHERE lfa.flaw=",
            $f,
            " AND lfm.merged_into IS NULL) OR EXISTS (SELECT 1 FROM candidate_flaw lfb WHERE \
             lfb.flaw=",
            $f,
            " AND ",
            live_candidate!("lfb.candidate"),
            ")))"
        )
    };
}
pub(super) use live_flaw;

/// A live flaw depends on premise `$p`, or an unretracted path or a recorded state's check
/// relies on it, or on a premise resting on it at any remove, and that flaw, path or state is not
/// outside the scope.
macro_rules! relied {
    ($p:expr) => {
        concat!(
            "EXISTS (WITH RECURSIVE above(node) AS (SELECT ",
            $p,
            " UNION SELECT sb.premise FROM premise_basis sb JOIN above ON sb.rests_on=above.node) \
             SELECT 1 FROM above WHERE EXISTS (SELECT 1 FROM flaw_premise sf WHERE \
             sf.premise=above.node AND ",
            live_flaw!("sf.flaw"),
            " AND NOT ",
            $crate::store::work::scope::flaw_outside!("sf.flaw"),
            ") OR EXISTS (SELECT 1 FROM path_premise sp JOIN path sq ON sq.node=sp.path AND \
             sq.status!='retracted' WHERE sp.premise=above.node AND NOT ",
            $crate::store::work::scope::unscoped!("sp.path"),
            ") OR EXISTS (SELECT 1 FROM state_premise ss JOIN state st ON st.node=ss.state AND \
             st.status='recorded' WHERE ss.premise=above.node AND NOT ",
            $crate::store::work::scope::state_outside!("ss.state"),
            "))"
        )
    };
}
pub(super) use relied;

/// Premise `$p` owes a settling look: it is about the code, undecided, disputed since its
/// decision, or decided while a premise it rests on was decided or disputed since its settle
/// began, or since the decision itself when a review recorded it, and work [`relied`] on it is
/// live. A deployment premise is the user's to answer.
macro_rules! settle_owed {
    ($p:expr) => {
        concat!(
            "(EXISTS (SELECT 1 FROM premise_standing so WHERE so.node=",
            $p,
            " AND so.scope='code' AND (so.status='undecided' OR ",
            $crate::store::ledger::standing::disputed!("so", "decided"),
            " OR ifnull((",
            $crate::store::ledger::standing::undermined!("so.node"),
            ")>coalesce(",
            $crate::store::ledger::standing::stamp!("settle", "so.decided", "so.node"),
            ",so.decided),0))) AND ",
            relied!($p),
            ")"
        )
    };
}
pub(super) use settle_owed;

/// Premise `$p`'s settling look is owed and not exhausted.
macro_rules! settle_pending {
    ($p:expr) => {
        pending!("settle", $p, settle_owed!($p))
    };
}
pub(super) use settle_pending;

/// Flaw `$f` owes a chain, unless only paths outside the scope reach it: it is suspected,
/// refuted but disputed since, or confirmed with something it rests on changed since, while it
/// is on no live candidate or carries a proposal its chain decides; or it is not refuted, leaves
/// a recorded state wrong, and no chain of it began at its current basis, so an access of that
/// state recorded since its last chain owes it once more.
macro_rules! chain_owed {
    ($f:expr) => {
        concat!(
            "EXISTS (SELECT 1 FROM flaw_standing co WHERE co.node=",
            $f,
            " AND NOT ",
            $crate::store::work::scope::flaw_outside!("co.node"),
            " AND ((CASE co.status WHEN 'suspected' THEN 1 WHEN 'refuted' THEN ",
            $crate::store::ledger::standing::disputed!("co", "judged"),
            " ELSE ifnull(",
            basis!("chain", "co.node"),
            ">",
            $crate::store::ledger::standing::stamp!("chain", "co.judged", "co.node"),
            ",0) END AND (co.proposed IS NOT NULL OR NOT EXISTS (SELECT 1 FROM candidate_flaw cc WHERE \
             cc.flaw=co.node AND ",
            live_candidate!("cc.candidate"),
            "))) OR (co.status!='refuted' AND EXISTS (SELECT 1 FROM flaw_state cs JOIN state ct ON \
             ct.node=cs.state AND ct.status='recorded' WHERE cs.flaw=co.node) AND ",
            basis!("chain", "co.node"),
            ">ifnull((SELECT max(cl.basis) FROM task_node cl WHERE cl.phase='chain' AND \
             cl.node=co.node),0))))"
        )
    };
}

/// The flaws on path `$p`: those in a component its steps enter.
macro_rules! path_flaws {
    ($p:expr) => {
        concat!(
            "SELECT pf.node FROM path_step ps JOIN flaw pf ON pf.component=ps.component WHERE \
             ps.path=",
            $p
        )
    };
}

/// The lowest live path among those of `$paths`, a statement over `hx.path`: a shared stage's
/// work goes with one path, the same each time.
macro_rules! first_path {
    ($paths:expr) => {
        concat!(
            "(SELECT min(hx.path) FROM (",
            $paths,
            ") hx JOIN path hq ON hq.node=hx.path AND hq.status!='retracted')"
        )
    };
}

/// The first path of the premise bound as `?1`, or of one resting on it at any remove: one such
/// a premise is a premise of, or one a flaw needing it is on. A premise's sub-claims go with it.
macro_rules! settle_path {
    () => {
        first_path!(concat!(
            "SELECT path FROM path_premise WHERE premise IN (",
            settle_path!(@above),
            ") UNION ALL SELECT hs.path FROM flaw_premise hfp JOIN flaw hf ON hf.node=hfp.flaw JOIN \
             path_step hs ON hs.component=hf.component WHERE hfp.premise IN (",
            settle_path!(@above),
            ")"
        ))
    };
    (@above) => {
        "WITH RECURSIVE above(node) AS (SELECT value FROM json_each(?1) UNION SELECT hb.premise \
         FROM premise_basis hb JOIN above ON hb.rests_on=above.node) SELECT node FROM above"
    };
}

/// Bundled by path: a premise goes with every ready one of its first path, its own and those of
/// the flaws on it, with those sharing a dependent flaw, and with what each of them rests on at
/// any remove.
pub(super) const SETTLE: Sql = phase_sql!(
    "settle",
    "premise",
    settle_owed!("n.id"),
    "1",
    "NULL",
    "SELECT value FROM json_each(?1) UNION SELECT premise FROM flaw_premise WHERE flaw IN \
     (SELECT value FROM json_each(?1))",
    concat!(
        "WITH RECURSIVE below(node) AS (SELECT value FROM json_each(?1) UNION SELECT sp.premise \
         FROM flaw_premise hp JOIN flaw_premise sp ON sp.flaw=hp.flaw WHERE hp.premise IN \
         (SELECT value FROM json_each(?1)) UNION SELECT premise FROM path_premise WHERE path=",
        settle_path!(),
        " UNION SELECT fp.premise FROM (",
        path_flaws!(settle_path!()),
        ") pff JOIN flaw_premise fp ON fp.flaw=pff.node UNION SELECT bb.rests_on FROM \
         premise_basis bb JOIN below ON bb.premise=below.node) SELECT node FROM below"
    )
);

/// Ready once its neighbourhood is settled: no path stepping through its component or reading a
/// state it leaves wrong owes a sweep or a probe, and no such state a check. A reader recorded
/// later owes it once more. Bundled by path: a flaw goes with every ready one on its first path.
pub(super) const CHAIN: Sql = phase_sql!(
    "chain",
    "flaw",
    chain_owed!("n.id"),
    concat!(
        "NOT EXISTS (SELECT 1 FROM flaw rf JOIN path_step rp ON rp.component=rf.component WHERE \
         rf.node=n.id AND ",
        structure_pending!("rp.path"),
        ") AND NOT EXISTS (SELECT 1 FROM flaw_state rw WHERE rw.flaw=n.id AND (",
        check_pending!("rw.state"),
        " OR EXISTS (SELECT 1 FROM path_state rr WHERE rr.state=rw.state AND rr.access='read' \
         AND ",
        structure_pending!("rr.path"),
        ")))"
    ),
    "NULL",
    "SELECT value FROM json_each(?1)",
    concat!(
        "SELECT value FROM json_each(?1) UNION ",
        path_flaws!(first_path!(
            "SELECT hs.path AS path FROM flaw hf JOIN path_step hs ON hs.component=hf.component \
             WHERE hf.node IN (SELECT value FROM json_each(?1))"
        ))
    )
);

#[cfg(test)]
mod tests {
    use skyhook::job::JobState;

    use super::*;
    use crate::store::tests::{
        HIGH, access, assess, begun, candidate, check, child_agent, component, decide, end_job,
        fixture, flaw, judge, last_event, location, marks, path, premise, scenario, state, sweep,
        text, verdict,
    };
    use crate::store::work::queues::tests::{exhaust, queued};

    /// A code premise is owed a settling look while a live flaw or path relies on it, and again
    /// once disputed or edited after its decision, but not by its own settling look's edit; a
    /// merge into it is an edit whoever makes it. A deployment premise is never offered: it is
    /// asked of the user while a review waits on it.
    #[test]
    fn premises_are_settled_while_live_work_relies_on_them() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let (code, deployment, loose) = (
            premise(&store, &actor, PremiseScope::Code),
            premise(&store, &actor, PremiseScope::Deployment),
            premise(&store, &actor, PremiseScope::Code),
        );
        let lone = flaw(&store, &actor, scenario.component);
        let links = [
            Edge::FlawPremise {
                flaw: scenario.flaw,
                premise: code,
            },
            Edge::FlawPremise {
                flaw: scenario.flaw,
                premise: deployment,
            },
            Edge::FlawPremise {
                flaw: lone,
                premise: loose,
            },
        ];
        store.link(&actor, &links).unwrap();
        let settle = || queued(&store, Phase::Settle);
        assert_eq!(settle(), [NodeId::from(code), loose.into()]);
        judge(&store, lone, FlawStatus::Refuted);
        assert_eq!(
            settle(),
            [NodeId::from(code)],
            "a refuted flaw relies on nothing"
        );
        let path_premise = Edge::PathPremise {
            path: scenario.path,
            premise: loose,
        };
        store.link(&actor, &[path_premise]).unwrap();
        assert_eq!(
            settle(),
            [NodeId::from(code), loose.into()],
            "a path relies on it"
        );

        decide(&store, code, PremiseStatus::Holds);
        assert_eq!(settle(), [NodeId::from(loose)]);
        let edit = PremiseEdit {
            id: code,
            title: None,
            body: Some(text("checked before every copy")),
            scope: None,
            location: None,
        };
        store
            .upsert(&actor, &Upsert::Edit(NodeEdit::Premise(edit)))
            .unwrap();
        assert_eq!(
            settle(),
            [NodeId::from(code), loose.into()],
            "an edit since its decision reopens it"
        );
        let (settler, job, _) = begun(&store, &Begin::Settle(Listed::of([code])));
        let decided = StatusChange::Premise {
            id: code,
            status: PremiseStatus::Holds,
        };
        store
            .set_status(&settler, decided, &text("checked"))
            .unwrap();
        let edit = PremiseEdit {
            id: code,
            title: None,
            body: Some(text("checked before each copy")),
            scope: None,
            location: None,
        };
        store
            .upsert(&settler, &Upsert::Edit(NodeEdit::Premise(edit)))
            .unwrap();
        end_job(&store, job);
        assert_eq!(settle(), [NodeId::from(loose)], "its own edit");
        let merge = Merge {
            node: premise(&store, &actor, PremiseScope::Code).into(),
            into: code.into(),
            note: text("the same check"),
        };
        store.merge(&settler, &merge).unwrap();
        assert_eq!(settle(), [NodeId::from(code), loose.into()], "a merge");
        decide(&store, code, PremiseStatus::Holds);
        assert!(
            marks(&store, last_event(&store)).contains(&(code.into(), EventEffect::Changed)),
            "deciding it again changes it"
        );
        let dispute = NewEvidence {
            location: None,
            environment: None,
            stance: Stance::Contradicts,
            note: text("one caller skips the check"),
        };
        store.evidence(&actor, code.into(), &[dispute]).unwrap();
        assert_eq!(
            settle(),
            [NodeId::from(code), loose.into()],
            "a dispute reopens it"
        );

        assess(&store, scenario.candidate, HIGH);
        let asked = store.synthesis(&[]).unwrap().asked;
        assert_eq!(
            asked.iter().map(|asked| asked.premise).collect::<Vec<_>>(),
            [deployment]
        );
    }

    /// A status recorded outside the work owning a premise that agrees with its standing
    /// restates it, concerning it alone and owing nothing; one that disagrees disputes it, moving
    /// its settle alone, whose next task is told of it and resolves it by deciding. Once a
    /// settle decided after it, repeating it only concerns the premise while that decision
    /// stands on the settle's current basis: after an edit, a repeat answered before disputes it
    /// again, as does a status not yet proposed.
    #[test]
    fn a_proposal_disputes_a_standing_only_when_it_disagrees() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let code = premise(&store, &actor, PremiseScope::Code);
        let rests = Edge::FlawPremise {
            flaw: scenario.flaw,
            premise: code,
        };
        store.link(&actor, &[rests]).unwrap();
        decide(&store, code, PremiseStatus::Holds);
        let settle = || queued(&store, Phase::Settle);
        let view = || {
            let NodeView::Premise(view) = store.node(code.into()).unwrap() else {
                panic!("a premise")
            };
            view
        };
        let (proposer, job, _) = begun(&store, &Begin::Chain(Listed::of([scenario.flaw])));
        let propose = |status| {
            let change = StatusChange::Premise { id: code, status };
            store
                .set_status(&proposer, change, &text("reviewed"))
                .unwrap();
            marks(&store, last_event(&store))
        };
        assert_eq!(
            propose(PremiseStatus::Holds),
            [(code.into(), EventEffect::Concerns)]
        );
        assert!(
            settle().is_empty() && view().proposed.is_none(),
            "a restatement"
        );
        assert_eq!(
            propose(PremiseStatus::Fails),
            [(code.into(), EventEffect::Disputes)]
        );
        end_job(&store, job);
        assert_eq!(settle(), [NodeId::from(code)]);
        let disputed = view();
        let proposed = disputed.proposed.unwrap();
        assert_eq!(
            (
                disputed.status,
                disputed.disputed,
                proposed.value,
                &proposed.agent
            ),
            (PremiseStatus::Holds, true, PremiseStatus::Fails, &proposer)
        );

        let (settler, job) = child_agent(&store, JobState::Running);
        let begun = store
            .begin(&settler, &Begin::Settle(Listed::of([code])))
            .unwrap();
        assert_eq!(
            (begun.proposals.iter())
                .map(|subject| (subject.node, &subject.proposal.value))
                .collect::<Vec<_>>(),
            [(NodeId::from(code), &Proposed::Premise(PremiseStatus::Fails))]
        );
        let holds = StatusChange::Premise {
            id: code,
            status: PremiseStatus::Holds,
        };
        store.set_status(&settler, holds, &text("traced")).unwrap();
        end_job(&store, job);
        assert!(settle().is_empty(), "its settle decided it");

        assert_eq!(
            propose(PremiseStatus::Fails),
            [(code.into(), EventEffect::Concerns)]
        );
        assert!(
            settle().is_empty() && view().proposed.is_none(),
            "an answered repeat"
        );
        let edit = PremiseEdit {
            id: code,
            title: None,
            body: Some(text("checked before every copy")),
            scope: None,
            location: None,
        };
        store
            .upsert(&actor, &Upsert::Edit(NodeEdit::Premise(edit)))
            .unwrap();
        decide(&store, code, PremiseStatus::Fails);
        let dispute = NewEvidence {
            location: None,
            environment: None,
            stance: Stance::Contradicts,
            note: text("one caller skips the check"),
        };
        store.evidence(&actor, code.into(), &[dispute]).unwrap();
        decide(&store, code, PremiseStatus::Holds);
        assert_eq!(
            propose(PremiseStatus::Fails),
            [(code.into(), EventEffect::Disputes)],
            "answered before the premise moved"
        );
        assert_eq!(
            propose(PremiseStatus::Undecided),
            [(code.into(), EventEffect::Disputes)]
        );
        assert_eq!(settle(), [NodeId::from(code)]);
    }

    /// A premise a relied-on premise rests on is owed a settling look as that one is. A decided
    /// premise is owed one again once a premise it rests on is decided, either way, or disputed
    /// after its decision, but not for a decision it was decided after, nor for one its own
    /// settle recorded.
    #[test]
    fn a_decided_premise_reopens_when_what_it_rests_on_changes() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let [parent, sub] = [(); 2].map(|_| premise(&store, &actor, PremiseScope::Code));
        let links = [
            Edge::FlawPremise {
                flaw: scenario.flaw,
                premise: parent,
            },
            Edge::PremiseBasis {
                premise: parent,
                rests_on: sub,
            },
        ];
        store.link(&actor, &links).unwrap();
        let settle = || queued(&store, Phase::Settle);
        let decide = |premise, status| decide(&store, premise, status);
        assert_eq!(settle(), [NodeId::from(parent), sub.into()]);
        decide(sub, PremiseStatus::Holds);
        decide(parent, PremiseStatus::Holds);
        assert!(settle().is_empty(), "decided after what it rests on");
        decide(sub, PremiseStatus::Fails);
        assert_eq!(settle(), [NodeId::from(parent)], "what it rests on fails");
        decide(parent, PremiseStatus::Fails);
        assert!(settle().is_empty(), "decided after the failure");
        decide(sub, PremiseStatus::Holds);
        assert_eq!(settle(), [NodeId::from(parent)], "what it rests on holds");
        decide(parent, PremiseStatus::Holds);
        let dispute = NewEvidence {
            location: None,
            environment: None,
            stance: Stance::Contradicts,
            note: text("one exit skips the check"),
        };
        store.evidence(&actor, sub.into(), &[dispute]).unwrap();
        assert_eq!(
            settle(),
            [NodeId::from(parent), sub.into()],
            "what it rests on is disputed"
        );
        let (settler, job, _) = begun(&store, &Begin::Settle(Listed::of([parent, sub])));
        for premise in [parent, sub] {
            let change = StatusChange::Premise {
                id: premise,
                status: PremiseStatus::Holds,
            };
            store.set_status(&settler, change, &text("traced")).unwrap();
        }
        end_job(&store, job);
        assert!(settle().is_empty(), "its own settle's decisions");
    }

    /// A flaw is owed a chain while no live candidate holds it, once the paths through its
    /// component and the readers and checks of the states it leaves wrong are settled, whatever
    /// a path elsewhere owes; a flaw only an excluded or disproved candidate holds, or one leaving
    /// a chain, is owed again with fresh tries, and a
    /// confirmed one only once something it rests on changes, during its chain included. An
    /// edit within its chain leaves its judgement standing.
    #[test]
    fn flaws_are_chained_until_a_live_candidate_holds_them() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let loose = flaw(&store, &actor, scenario.component);
        let chain = || queued(&store, Phase::Chain);
        assert!(
            chain().is_empty(),
            "the path through its component is unswept"
        );
        assert_eq!(store.run_facts().unwrap().summary.owed(Phase::Chain), 1);
        let elsewhere = component(&store, &actor, "renderer");
        let consumer = path(&store, &actor, "renderer", &[elsewhere]);
        sweep(&store, scenario.path);
        assert_eq!(
            chain(),
            [NodeId::from(loose)],
            "a path elsewhere holds nothing"
        );
        let window = state(&store, &actor, NewState::Instance);
        let read = access(
            consumer,
            window,
            Access::Read,
            location("renderer/lib.c", 3),
        );
        let wrong = Edge::FlawState {
            flaw: loose,
            state: window,
        };
        store.link(&actor, &[read, wrong]).unwrap();
        assert!(
            chain().is_empty(),
            "a reader of the state it leaves wrong is unswept"
        );
        sweep(&store, consumer);
        assert!(chain().is_empty(), "the state it leaves wrong is unchecked");
        let basis = premise(&store, &actor, PremiseScope::Code);
        check(&store, window, &[basis]);
        assert_eq!(chain(), [NodeId::from(loose)]);
        let unwrong = Edge::FlawState {
            flaw: loose,
            state: window,
        };
        store.unlink(&actor, &[unwrong]).unwrap();
        let held = candidate(&store, &actor, scenario.path, &[loose]);
        assert!(chain().is_empty());
        let deployed = premise(&store, &actor, PremiseScope::Deployment);
        let needs = Edge::FlawPremise {
            flaw: loose,
            premise: deployed,
        };
        store.link(&actor, &[needs]).unwrap();
        let answered = StatusChange::Premise {
            id: deployed,
            status: PremiseStatus::Fails,
        };
        store
            .set_status(&actor, answered, &text("not deployed"))
            .unwrap();
        assess(&store, held, HIGH);
        verdict(&store, held, Verdict::Excluded);
        assert_eq!(
            chain(),
            [NodeId::from(loose)],
            "an excluded candidate holds nothing"
        );
        let gone = premise(&store, &actor, PremiseScope::Code);
        let needs = Edge::FlawPremise {
            flaw: loose,
            premise: gone,
        };
        store.link(&actor, &[needs]).unwrap();
        decide(&store, gone, PremiseStatus::Fails);
        verdict(&store, held, Verdict::Disproved);
        assert_eq!(
            chain(),
            [NodeId::from(loose)],
            "a disproved candidate holds nothing"
        );

        let apart = flaw(&store, &actor, scenario.component);
        let confirmed = StatusChange::Flaw {
            id: apart,
            status: FlawStatus::Confirmed,
        };
        let (questioner, job, _) = begun(&store, &Begin::Chain(Listed::of([apart])));
        store
            .set_status(&questioner, confirmed.clone(), &text("no consequence"))
            .unwrap();
        let edit = FlawEdit {
            id: apart,
            title: None,
            body: Some(text("the copy is bounded by its caller")),
            component: None,
            location: None,
            cwe: None,
        };
        store
            .upsert(&questioner, &Upsert::Edit(NodeEdit::Flaw(edit)))
            .unwrap();
        end_job(&store, job);
        assert_eq!(chain(), [NodeId::from(loose)], "its chain's own edit");
        let (questioner, job, _) = begun(&store, &Begin::Chain(Listed::of([apart])));
        let rests = Edge::FlawPremise {
            flaw: apart,
            premise: premise(&store, &actor, PremiseScope::Code),
        };
        store.link(&actor, &[rests]).unwrap();
        store
            .set_status(&questioner, confirmed, &text("no consequence"))
            .unwrap();
        end_job(&store, job);
        assert_eq!(
            chain(),
            [NodeId::from(loose), apart.into()],
            "what it rests on changed while it was chained"
        );

        let judge = |flaw, status| judge(&store, flaw, status);
        let decide = |premise| decide(&store, premise, PremiseStatus::Holds);
        let (confirmed, refuted) = (
            flaw(&store, &actor, scenario.component),
            flaw(&store, &actor, scenario.component),
        );
        let code = premise(&store, &actor, PremiseScope::Code);
        let links = [confirmed, refuted].map(|flaw| Edge::FlawPremise {
            flaw,
            premise: code,
        });
        store.link(&actor, &links).unwrap();
        judge(confirmed, FlawStatus::Confirmed);
        judge(refuted, FlawStatus::Refuted);
        judge(apart, FlawStatus::Confirmed);
        assert_eq!(chain(), [NodeId::from(loose)]);
        decide(code);
        assert_eq!(
            chain(),
            [NodeId::from(loose), confirmed.into()],
            "a decided premise re-owes the confirmed flaw resting on it"
        );
        let dispute = NewEvidence {
            location: None,
            environment: None,
            stance: Stance::Contradicts,
            note: text("the bound is skipped on retry"),
        };
        store.evidence(&actor, refuted.into(), &[dispute]).unwrap();
        decide(code);
        assert_eq!(
            chain(),
            [NodeId::from(loose), confirmed.into(), refuted.into()],
            "a premise decision leaves the dispute standing"
        );

        let edited = flaw(&store, &actor, scenario.component);
        judge(edited, FlawStatus::Refuted);
        let edit = FlawEdit {
            id: edited,
            title: None,
            body: Some(text("the bound is skipped on every retry")),
            component: None,
            location: None,
            cwe: None,
        };
        store
            .upsert(&actor, &Upsert::Edit(NodeEdit::Flaw(edit)))
            .unwrap();
        assert_eq!(
            chain(),
            [
                NodeId::from(loose),
                confirmed.into(),
                refuted.into(),
                edited.into()
            ],
            "an edit since its judgement reads it suspected"
        );
        exhaust(&store, &Begin::Chain(Listed::of([loose])));
        let behind = candidate(&store, &actor, scenario.path, &[confirmed, loose]);
        assess(&store, behind, HIGH);
        verdict(&store, behind, Verdict::Disproved);
        assert_eq!(
            chain()[..2],
            [NodeId::from(loose), confirmed.into()],
            "a disproval releases a flaw held behind the root with fresh tries"
        );
        exhaust(&store, &Begin::Chain(Listed::of([loose])));
        let holder = candidate(&store, &actor, scenario.path, &[scenario.flaw, loose]);
        assert!(!chain().contains(&loose.into()));
        let edit = CandidateEdit {
            id: holder,
            title: None,
            body: None,
            actions: None,
            chain: Some(vec![scenario.flaw].try_into().unwrap()),
        };
        store
            .upsert(&actor, &Upsert::Edit(NodeEdit::Candidate(edit)))
            .unwrap();
        assert_eq!(
            chain()[0],
            NodeId::from(loose),
            "leaving a chain behind its root releases it with fresh tries"
        );
    }

    /// A flaw leaving a state wrong is owed a chain, even while a live candidate holds it, until
    /// one begins at its basis, however many paths read the state; an access of the state
    /// recorded since owes it once more. A refuted flaw is owed none. A chain that replaces its
    /// flaw with a new one merged into it owes the new one a chain, resting on the merge.
    #[test]
    fn a_chain_is_owed_once_per_move_of_its_states_readers() {
        let (_session, store, actor) = fixture();
        let [decoder, consumer] =
            ["decoder", "consumer"].map(|name| component(&store, &actor, name));
        let writer = path(&store, &actor, "writer", &[decoder]);
        let [first, second, later] =
            ["first", "second", "later"].map(|name| path(&store, &actor, name, &[consumer]));
        let window = state(&store, &actor, NewState::Instance);
        let at = location("decoder/lib.c", 3);
        let mut links = vec![access(writer, window, Access::Write, at.clone())];
        links
            .extend([first, second].map(|reader| access(reader, window, Access::Read, at.clone())));
        store.link(&actor, &links).unwrap();
        for path in [writer, first, second, later] {
            sweep(&store, path);
        }
        let basis = premise(&store, &actor, PremiseScope::Code);
        check(&store, window, &[basis]);
        let stale = flaw(&store, &actor, decoder);
        candidate(&store, &actor, writer, &[stale]);
        let chain = || queued(&store, Phase::Chain);
        assert!(chain().is_empty(), "a held flaw leaving nothing wrong");
        let wrong = Edge::FlawState {
            flaw: stale,
            state: window,
        };
        store.link(&actor, &[wrong]).unwrap();
        assert_eq!(chain(), [NodeId::from(stale)]);
        let (_, job, _) = begun(&store, &Begin::Chain(Listed::of([stale])));
        end_job(&store, job);
        assert!(chain().is_empty(), "its chain began at its basis");
        let read = access(later, window, Access::Read, at);
        store.link(&actor, &[read]).unwrap();
        check(&store, window, &[basis]);
        assert_eq!(chain(), [NodeId::from(stale)], "a reader recorded since");
        judge(&store, stale, FlawStatus::Refuted);
        assert!(chain().is_empty(), "a refuted flaw");
        let replaced = flaw(&store, &actor, consumer);
        let (questioner, job, _) = begun(&store, &Begin::Chain(Listed::of([replaced])));
        let replacement = flaw(&store, &questioner, consumer);
        let merge = Merge {
            node: replaced.into(),
            into: replacement.into(),
            note: text("restated"),
        };
        store.merge(&questioner, &merge).unwrap();
        end_job(&store, job);
        assert_eq!(chain(), [NodeId::from(replacement)]);
        assert_eq!(store.run_facts().unwrap().summary.owed(Phase::Chain), 1);
    }
}

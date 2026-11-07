//! Structure work: paths owing a sweep or a probe, states owing a check across calls, premises
//! owing a settling look and flaws owing a chain.

use super::*;

/// Path `$p` is unswept, enters within the scope and reads some recorded state its caller
/// supplies, an input or a handed one such as an instance handle: a correction of a swept path
/// owes nothing, and a path reading none, or only retracted ones, is no flow to sweep.
macro_rules! unswept {
    ($p:expr) => {
        concat!(
            "(SELECT status='unswept' FROM path WHERE node=",
            $p,
            ") AND NOT ",
            $crate::store::scope::unscoped!($p),
            " AND EXISTS (SELECT 1 FROM ",
            $crate::store::bearing::driven_reads!("us", $p),
            ")"
        )
    };
}
pub(super) use unswept;

/// None of path `$p`'s reads of recorded caller-supplied states sits in code: its sweep is a
/// probe, which observes or disassembles what it cannot read.
macro_rules! probed {
    ($p:expr) => {
        concat!(
            "NOT EXISTS (SELECT 1 FROM ",
            $crate::store::bearing::driven_reads!("xs", $p),
            " JOIN code_location xc ON xc.location=xs.location)"
        )
    };
}
pub(in crate::store) use probed;

/// Interface `$i` is served for live work: the launch does live work, and an environment for no
/// particular candidate serves it.
macro_rules! served {
    ($i:expr) => {
        concat!(
            "((SELECT live FROM run)!='no' AND EXISTS (SELECT 1 FROM environment_interface xe \
             JOIN environment xv ON xv.id=xe.environment WHERE xe.interface=",
            $i,
            " AND xv.candidate IS NULL))"
        )
    };
}
pub(in crate::store) use served;

/// A read of a recorded caller-supplied state on path `$p` sits on an interface that is not
/// served.
macro_rules! uncovered {
    ($p:expr) => {
        concat!(
            "EXISTS (SELECT 1 FROM ",
            $crate::store::bearing::driven_reads!("xs", $p),
            " JOIN interface_location xi ON xi.location=xs.location WHERE NOT ",
            served!("xi.interface"),
            ")"
        )
    };
}
pub(super) use uncovered;

/// Path `$p` owes a sweep: it is unswept and one of its input reads sits in code.
macro_rules! sweep_owed {
    ($p:expr) => {
        concat!("(", unswept!($p), " AND NOT ", probed!($p), ")")
    };
}
pub(super) use sweep_owed;

/// Path `$p` owes a probe: it is unswept, none of its input reads sits in code, and every
/// interface they sit on is served.
macro_rules! probe_owed {
    ($p:expr) => {
        concat!(
            "(",
            unswept!($p),
            " AND ",
            probed!($p),
            " AND NOT ",
            uncovered!($p),
            ")"
        )
    };
}
pub(super) use probe_owed;

/// Path `$p`'s sweep is owed and not exhausted.
macro_rules! sweep_pending {
    ($p:expr) => {
        pending!("sweep", $p, sweep_owed!($p))
    };
}
pub(super) use sweep_pending;

/// Path `$p`'s sweep or probe is owed and not exhausted.
macro_rules! structure_pending {
    ($p:expr) => {
        concat!(
            "(",
            sweep_pending!($p),
            " OR ",
            pending!("probe", $p, probe_owed!($p)),
            ")"
        )
    };
}
pub(in crate::store) use structure_pending;

/// The state keying path `$p`'s sweep bundle: of the recorded handed states it reads or writes,
/// the one the most unretracted paths access, the instance handle every call on that instance
/// shares, so those calls bundle together whatever order their states were recorded in; else
/// the recorded instance state the most such paths access; none when it accesses neither. Ties
/// go to the lowest.
macro_rules! key_state {
    ($p:expr) => {
        concat!(
            "coalesce(",
            key_state!(@ concat!("=", $p), "handed"),
            ",",
            key_state!(@ concat!("=", $p), "instance"),
            ")"
        )
    };
    // Of the recorded `$scope` states the paths `ks.path$paths` selects access, the one the most
    // unretracted paths access, ties to the lowest.
    (@ $paths:expr, $scope:literal) => {
        concat!(
            "(SELECT ks.state FROM path_state ks JOIN state kt ON kt.node=ks.state AND \
             kt.scope='",
            $scope,
            "' AND kt.status='recorded' WHERE ks.path",
            $paths,
            " GROUP BY ks.state ORDER BY (SELECT count(DISTINCT kc.path) FROM path_state kc JOIN path \
             kq ON kq.node=kc.path AND kq.status!='retracted' WHERE kc.state=ks.state) DESC,ks.state \
             LIMIT 1)"
        )
    };
}

/// The state keying state `$s`'s check bundle: a handed state itself; an instance state, the
/// handed state its unretracted writer paths access that the most such paths access, as a path's
/// sweep is keyed, so the value a call hands back is checked with what its producers leave in the
/// instance the next call resumes from; else the state itself.
macro_rules! check_key {
    ($s:expr) => {
        concat!(
            "coalesce((SELECT kh.node FROM state kh WHERE kh.node=",
            $s,
            " AND kh.scope='handed'),",
            key_state!(@ concat!(
                " IN (SELECT kw.path FROM path_state kw JOIN path kp ON kp.node=kw.path AND \
                 kp.status!='retracted' WHERE kw.state=",
                $s,
                " AND kw.access='write')"
            ), "handed"),
            ",",
            $s,
            ")"
        )
    };
}

/// State `$s`'s check recorded at event `$event` is stale: the state was accessed, edited or
/// merged into since. A check recorded outside one of its own stands on nothing, and reads
/// null.
macro_rules! check_stale {
    ($s:expr, $event:expr) => {
        concat!(
            $crate::store::bearing::basis!("check", $s),
            ">",
            $crate::store::bearing::stamp!("check", $event, $s)
        )
    };
}
pub(in crate::store) use check_stale;

/// State `$s` owes a check across calls: a recorded instance or handed state never checked, or
/// whose check is stale, that not only paths outside the scope access.
macro_rules! check_owed {
    ($s:expr) => {
        concat!(
            "EXISTS (SELECT 1 FROM state co LEFT JOIN state_check cc ON cc.state=co.node WHERE \
             co.node=",
            $s,
            " AND co.scope IN ('instance','handed') AND co.status='recorded' AND ifnull(",
            check_stale!("co.node", "cc.event"),
            ",1) AND NOT ",
            $crate::store::scope::state_outside!("co.node"),
            ")"
        )
    };
}

/// State `$s`'s check is owed and not exhausted.
macro_rules! check_pending {
    ($s:expr) => {
        pending!("check", $s, check_owed!($s))
    };
}

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
            $crate::store::scope::flaw_outside!("sf.flaw"),
            ") OR EXISTS (SELECT 1 FROM path_premise sp JOIN path sq ON sq.node=sp.path AND \
             sq.status!='retracted' WHERE sp.premise=above.node AND NOT ",
            $crate::store::scope::unscoped!("sp.path"),
            ") OR EXISTS (SELECT 1 FROM state_premise ss JOIN state st ON st.node=ss.state AND \
             st.status='recorded' WHERE ss.premise=above.node AND NOT ",
            $crate::store::scope::state_outside!("ss.state"),
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
            $crate::store::bearing::disputed!("so", "decided"),
            " OR ifnull((",
            $crate::store::bearing::undermined!("so.node"),
            ")>coalesce(",
            $crate::store::bearing::stamp!("settle", "so.decided", "so.node"),
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
            $crate::store::scope::flaw_outside!("co.node"),
            " AND ((CASE co.status WHEN 'suspected' THEN 1 WHEN 'refuted' THEN ",
            $crate::store::bearing::disputed!("co", "judged"),
            " ELSE ifnull(",
            basis!("chain", "co.node"),
            ">",
            $crate::store::bearing::stamp!("chain", "co.judged", "co.node"),
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

/// Bundled by state: a path goes with every ready one sharing the state its sweep's bundle is
/// keyed on, and alone when it accesses none.
pub(super) const SWEEP: Sql = phase_sql!(
    "sweep",
    "path",
    sweep_owed!("n.id"),
    "1",
    "NULL",
    "NULL",
    concat!(
        "SELECT value FROM json_each(?1) UNION SELECT bs.path FROM path_state bs WHERE bs.state=",
        key_state!("(SELECT value FROM json_each(?1))"),
        " AND ",
        key_state!("bs.path"),
        "=bs.state"
    )
);

pub(super) const PROBE: Sql = phase_sql!(
    "probe",
    "path",
    probe_owed!("n.id"),
    "1",
    "NULL",
    "NULL",
    "NULL"
);

/// Ready once no path recorded by its basis owes a pending sweep, accessing it or not: a path
/// recorded since links its accesses when swept, which owe the check again. Bundled by state: a
/// state goes with every ready one sharing the state its check's bundle is keyed on, the instance
/// states written on the paths accessing a handed state with it.
pub(super) const CHECK: Sql = phase_sql!(
    "check",
    "state",
    check_owed!("n.id"),
    concat!(
        "ifnull((SELECT min((SELECT min(rc.event) FROM event_node rc WHERE rc.node=rk.node)) \
         FROM path rk WHERE ",
        sweep_pending!("rk.node"),
        ")>",
        basis!("check", "n.id"),
        ",1)"
    ),
    "NULL",
    "NULL",
    concat!(
        "SELECT value FROM json_each(?1) UNION SELECT ",
        check_key!("(SELECT value FROM json_each(?1))"),
        " UNION SELECT bw.state FROM path_state bh JOIN path_state bw ON bw.path=bh.path AND \
         bw.access='write' WHERE bh.state=",
        check_key!("(SELECT value FROM json_each(?1))"),
        " AND ",
        check_key!("bw.state"),
        "=bh.state"
    )
);

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

    use super::{
        super::{
            super::tests::{
                HIGH, access, assess, begun, candidate, check, child_agent, component, daemon,
                decide, end_job, fixture, fixture_under, flaw, judge, last_event, location, marks,
                operation, path, path_from, premise, scenario, serve, state, sweep, text,
                validated, verdict,
            },
            tests::{exhaust, queued, status},
        },
        *,
    };

    /// A path is owed one sweep: two tasks leaving it unswept exhaust it, with their child's
    /// reply, until it is marked unswept or its path changes; once swept, a later correction
    /// owes nothing. An unswept mark leaves its candidates' settled work settled. A duplicate
    /// path whose input read is moved to the path it repeats owes nothing.
    #[test]
    fn a_path_is_swept_once() {
        let (_session, store, actor) = fixture();
        let scenario = validated(&store, &actor, "decoder");
        let route = NodeId::from(scenario.path);
        let sweep_work = Begin::Sweep(vec![scenario.path]);
        exhaust(&store, &sweep_work);
        let unswept = StatusChange::Path(scenario.path, PathStatus::Unswept);
        store
            .set_status(&actor, unswept, &text("cut short"))
            .unwrap();
        assert_eq!(
            queued(&store, Phase::Sweep),
            [route],
            "an unswept mark offers the sweep again"
        );
        assert_eq!(
            status(&store, scenario.candidate),
            Status::Validated,
            "its candidate's review stays settled"
        );
        exhaust(&store, &sweep_work);
        let exhausted = store.synthesis(&[]).unwrap().exhausted;
        assert_eq!(
            exhausted
                .iter()
                .map(|entry| (entry.node, entry.reply.as_deref()))
                .collect::<Vec<_>>(),
            [(route, None)]
        );
        let correct = |note: &str| {
            let step = Link::PathStep {
                path: scenario.path,
                component: scenario.component,
                location: None,
                note: text(note),
                position: None,
            };
            store.link(&actor, &[step]).unwrap();
        };
        correct("returns");
        assert_eq!(
            queued(&store, Phase::Sweep),
            [route],
            "a corrected path is offered again"
        );
        sweep(&store, scenario.path);
        correct("loops");
        assert!(
            queued(&store, Phase::Sweep).is_empty(),
            "a swept path stays swept"
        );

        let duplicate = path(&store, &actor, "decoder again", &[scenario.component]);
        assert_eq!(queued(&store, Phase::Sweep), [NodeId::from(duplicate)]);
        let NodeView::Path(view) = store.node(duplicate.into()).unwrap() else {
            panic!("a path")
        };
        let read = &view.states[0];
        let unlink = Unlink::PathState {
            path: duplicate,
            state: read.id,
            access: read.access,
            location: read.location.clone(),
        };
        store.unlink(&actor, &[unlink]).unwrap();
        let read = access(scenario.path, read.id, read.access, read.location.clone());
        store.link(&actor, &[read]).unwrap();
        assert!(
            queued(&store, Phase::Sweep).is_empty(),
            "a path reading no input owes no sweep"
        );
        let at = location("decoder again/net.c", 2);
        let [inner, handle] =
            [NewState::Instance, NewState::Handed].map(|scope| state(&store, &actor, scope));
        store
            .link(
                &actor,
                &[access(duplicate, inner, Access::Read, at.clone())],
            )
            .unwrap();
        assert!(
            queued(&store, Phase::Sweep).is_empty(),
            "a path reading only its instance's state owes no sweep"
        );
        store
            .link(&actor, &[access(duplicate, handle, Access::Read, at)])
            .unwrap();
        assert_eq!(
            queued(&store, Phase::Sweep),
            [NodeId::from(duplicate)],
            "a path driven by a handle its caller holds owes a sweep"
        );
    }

    /// A retracted path owes no sweep, exhausted or not, and counts nowhere, the component only
    /// it reaches unreached; the premises it relies on owe no settling look, ask nothing of the
    /// user and hold back no review of the candidate performing it. Marked unswept again, it is
    /// offered its sweep afresh, and the premises it relies on are back in its candidates'
    /// chains, so a verdict and report made while it was retracted no longer stand.
    #[test]
    fn a_retracted_path_owes_nothing_until_restored() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let (code, deployment) = (
            premise(&store, &actor, PremiseScope::Code),
            premise(&store, &actor, PremiseScope::Deployment),
        );
        let relies = [code, deployment].map(|premise| Link::PathPremise {
            path: scenario.path,
            premise,
        });
        store.link(&actor, &relies).unwrap();
        exhaust(&store, &Begin::Sweep(vec![scenario.path]));
        assess(&store, scenario.candidate, HIGH);
        let relied = || {
            let asked = store.synthesis(&[]).unwrap().asked;
            let review = queued(&store, Phase::Review);
            (queued(&store, Phase::Settle), asked.len(), review)
        };
        assert_eq!(relied(), (vec![NodeId::from(code)], 1, vec![]));
        let mark = |status| {
            let change = StatusChange::Path(scenario.path, status);
            store.set_status(&actor, change, &text("in error")).unwrap();
        };
        mark(PathStatus::Retracted);
        assert!(queued(&store, Phase::Sweep).is_empty());
        assert_eq!(
            relied(),
            (vec![], 0, vec![NodeId::from(scenario.candidate)])
        );
        let summary = store.run_facts().unwrap().summary;
        assert_eq!(
            (summary.owed(Phase::Sweep), summary.exhausted(Phase::Sweep)),
            (0, 0)
        );
        let coverage = store.coverage().unwrap();
        assert_eq!((coverage.paths, coverage.unreached), (0, 1));
        verdict(&store, scenario.candidate, Verdict::Validated);
        let finding = store
            .report(&actor, scenario.flaw, &text("overflow"), &text("report"))
            .unwrap();
        let current = || {
            let NodeView::Candidate(candidate) = store.node(scenario.candidate.into()).unwrap()
            else {
                panic!("a candidate")
            };
            let NodeView::Finding(report) = store.node(finding.into()).unwrap() else {
                panic!("a finding")
            };
            (candidate.verdict.unwrap().current, report.current)
        };
        assert_eq!(current(), (true, true));
        mark(PathStatus::Unswept);
        assert_eq!(queued(&store, Phase::Sweep), [NodeId::from(scenario.path)]);
        assert_eq!(current(), (false, false));
    }

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
            Link::FlawPremise {
                flaw: scenario.flaw,
                premise: code,
            },
            Link::FlawPremise {
                flaw: scenario.flaw,
                premise: deployment,
            },
            Link::FlawPremise {
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
        let path_premise = Link::PathPremise {
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
        let edit = Edit {
            title: None,
            body: Some(text("checked before every copy")),
            detail: PremiseEdit::default(),
        };
        store
            .upsert(&actor, &Upsert::Premise(Change::Edit(code, edit)))
            .unwrap();
        assert_eq!(
            settle(),
            [NodeId::from(code), loose.into()],
            "an edit since its decision reopens it"
        );
        let (settler, job, _) = begun(&store, &Begin::Settle(vec![code]));
        let decided = StatusChange::Premise(code, PremiseStatus::Holds);
        store
            .set_status(&settler, decided, &text("checked"))
            .unwrap();
        let edit = Edit {
            title: None,
            body: Some(text("checked before each copy")),
            detail: PremiseEdit::default(),
        };
        store
            .upsert(&settler, &Upsert::Premise(Change::Edit(code, edit)))
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
        let rests = Link::FlawPremise {
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
        let (proposer, job, _) = begun(&store, &Begin::Chain(vec![scenario.flaw]));
        let propose = |status| {
            let change = StatusChange::Premise(code, status);
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
        let begun = store.begin(&settler, &Begin::Settle(vec![code])).unwrap();
        assert_eq!(
            (begun.proposals.iter())
                .map(|subject| (subject.node, &subject.proposal.value))
                .collect::<Vec<_>>(),
            [(NodeId::from(code), &Proposed::Premise(PremiseStatus::Fails))]
        );
        let holds = StatusChange::Premise(code, PremiseStatus::Holds);
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
        let edit = Edit {
            title: None,
            body: Some(text("checked before every copy")),
            detail: PremiseEdit::default(),
        };
        store
            .upsert(&actor, &Upsert::Premise(Change::Edit(code, edit)))
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
            Link::FlawPremise {
                flaw: scenario.flaw,
                premise: parent,
            },
            Link::PremiseBasis {
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
        let (settler, job, _) = begun(&store, &Begin::Settle(vec![parent, sub]));
        for premise in [parent, sub] {
            let change = StatusChange::Premise(premise, PremiseStatus::Holds);
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
        let wrong = Link::FlawState {
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
        let unwrong = Unlink::FlawState {
            flaw: loose,
            state: window,
        };
        store.unlink(&actor, &[unwrong]).unwrap();
        let held = candidate(&store, &actor, scenario.path, &[loose]);
        assert!(chain().is_empty());
        let deployed = premise(&store, &actor, PremiseScope::Deployment);
        let needs = Link::FlawPremise {
            flaw: loose,
            premise: deployed,
        };
        store.link(&actor, &[needs]).unwrap();
        let answered = StatusChange::Premise(deployed, PremiseStatus::Fails);
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
        let needs = Link::FlawPremise {
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
        let confirmed = StatusChange::Flaw(apart, FlawStatus::Confirmed);
        let (questioner, job, _) = begun(&store, &Begin::Chain(vec![apart]));
        store
            .set_status(&questioner, confirmed.clone(), &text("no consequence"))
            .unwrap();
        let edit = Edit {
            title: None,
            body: Some(text("the copy is bounded by its caller")),
            detail: FlawEdit::default(),
        };
        store
            .upsert(&questioner, &Upsert::Flaw(Change::Edit(apart, edit)))
            .unwrap();
        end_job(&store, job);
        assert_eq!(chain(), [NodeId::from(loose)], "its chain's own edit");
        let (questioner, job, _) = begun(&store, &Begin::Chain(vec![apart]));
        let rests = Link::FlawPremise {
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
        let links = [confirmed, refuted].map(|flaw| Link::FlawPremise {
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
        let edit = Edit {
            title: None,
            body: Some(text("the bound is skipped on every retry")),
            detail: FlawEdit::default(),
        };
        store
            .upsert(&actor, &Upsert::Flaw(Change::Edit(edited, edit)))
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
        exhaust(&store, &Begin::Chain(vec![loose]));
        let behind = candidate(&store, &actor, scenario.path, &[confirmed, loose]);
        assess(&store, behind, HIGH);
        verdict(&store, behind, Verdict::Disproved);
        assert_eq!(
            chain()[..2],
            [NodeId::from(loose), confirmed.into()],
            "a disproval releases a flaw held behind the root with fresh tries"
        );
        exhaust(&store, &Begin::Chain(vec![loose]));
        let holder = candidate(&store, &actor, scenario.path, &[scenario.flaw, loose]);
        assert!(!chain().contains(&loose.into()));
        let edit = Edit {
            title: None,
            body: None,
            detail: CandidateEdit {
                actions: None,
                chain: Some(vec![scenario.flaw].try_into().unwrap()),
            },
        };
        store
            .upsert(&actor, &Upsert::Candidate(Change::Edit(holder, edit)))
            .unwrap();
        assert_eq!(
            chain()[0],
            NodeId::from(loose),
            "leaving a chain behind its root releases it with fresh tries"
        );
    }

    /// An instance or handed state owes a check until one is recorded, waiting while a path
    /// recorded by its basis owes a sweep, accessing it or not, but not for one recorded since;
    /// it is owed again once an access is recorded outside its check, which a path recorded
    /// before it holds back in turn. An input or stored state owes none.
    #[test]
    fn a_state_is_checked_again_after_a_later_access() {
        let (_session, store, actor) = fixture();
        let decoder = component(&store, &actor, "decoder");
        let [apart, stream] =
            ["apart", "stream"].map(|name| path(&store, &actor, name, &[decoder]));
        let [window, session] =
            [NewState::Instance, NewState::Stored].map(|scope| state(&store, &actor, scope));
        let write = |line| {
            let at = location("decoder/lib.c", line);
            [window, session].map(|state| access(stream, state, Access::Write, at.clone()))
        };
        store.link(&actor, &write(3)).unwrap();
        let later = path(&store, &actor, "later", &[decoder]);
        let check = || queued(&store, Phase::Check);
        assert_eq!(store.run_facts().unwrap().summary.owed(Phase::Check), 1);
        sweep(&store, stream);
        assert!(
            check().is_empty(),
            "a path recorded before its basis owes a sweep"
        );
        sweep(&store, apart);
        assert_eq!(check(), [NodeId::from(window)], "a path recorded since");
        let (checker, job, _) = begun(&store, &Begin::Check(vec![window]));
        store.link(&checker, &write(4)).unwrap();
        let basis = premise(&store, &checker, PremiseScope::Code);
        let checked = StateMark::Checked {
            rests_on: Some(vec![basis]).try_into().unwrap(),
        };
        store
            .set_status(
                &checker,
                StatusChange::State(window, checked),
                &text("every writer meets every reader"),
            )
            .unwrap();
        end_job(&store, job);
        assert!(check().is_empty(), "its own check's accesses");
        store.link(&actor, &write(5)).unwrap();
        assert!(check().is_empty(), "a later access, past the unswept path");
        sweep(&store, later);
        assert_eq!(check(), [NodeId::from(window)]);
    }

    /// A check rests on the premises its mark names: each is owed its settle while undecided,
    /// and the check is owed again once one is decided failing or contradicted, but not when one
    /// holds.
    #[test]
    fn a_check_rests_on_the_premises_its_mark_names() {
        let (_session, store, actor) = fixture();
        let decoder = component(&store, &actor, "decoder");
        let stream = path(&store, &actor, "stream", &[decoder]);
        let window = state(&store, &actor, NewState::Instance);
        let write = access(stream, window, Access::Write, location("decoder/lib.c", 3));
        store.link(&actor, &[write]).unwrap();
        sweep(&store, stream);
        let [holding, failing] = [(); 2].map(|_| premise(&store, &actor, PremiseScope::Code));
        assert!(queued(&store, Phase::Settle).is_empty());
        check(&store, window, &[holding, failing]);
        assert_eq!(
            queued(&store, Phase::Settle),
            [NodeId::from(holding), failing.into()]
        );
        let rechecked = || queued(&store, Phase::Check);
        decide(&store, holding, PremiseStatus::Holds);
        assert!(rechecked().is_empty(), "a premise holding");
        decide(&store, failing, PremiseStatus::Fails);
        assert_eq!(rechecked(), [NodeId::from(window)], "a premise failing");
        check(&store, window, &[holding]);
        let dispute = NewEvidence {
            location: None,
            environment: None,
            stance: Stance::Contradicts,
            note: text("one caller skips the check"),
        };
        store.evidence(&actor, holding.into(), &[dispute]).unwrap();
        assert_eq!(
            rechecked(),
            [NodeId::from(window)],
            "a premise contradicted"
        );
    }

    /// A check rests on the history of each premise merged into one it names: a holding
    /// premise merged in owes it nothing, and a contradicted one owes it still once merged away.
    #[test]
    fn a_check_rests_on_its_premises_across_merges() {
        let (_session, store, actor) = fixture();
        let decoder = component(&store, &actor, "decoder");
        let stream = path(&store, &actor, "stream", &[decoder]);
        let window = state(&store, &actor, NewState::Instance);
        let write = access(stream, window, Access::Write, location("decoder/lib.c", 3));
        store.link(&actor, &[write]).unwrap();
        sweep(&store, stream);
        let [rested, restated, kept] = [(); 3].map(|_| {
            let premise = premise(&store, &actor, PremiseScope::Code);
            decide(&store, premise, PremiseStatus::Holds);
            premise
        });
        check(&store, window, &[rested]);
        let merge = |node: PremiseId, into: PremiseId| {
            let merge = Merge {
                node: node.into(),
                into: into.into(),
                note: text("the same claim"),
            };
            store.merge(&actor, &merge).unwrap();
        };
        let rechecked = || queued(&store, Phase::Check);
        merge(restated, rested);
        assert!(rechecked().is_empty(), "a holding premise merged in");
        let dispute = NewEvidence {
            location: None,
            environment: None,
            stance: Stance::Contradicts,
            note: text("one caller skips the check"),
        };
        store.evidence(&actor, rested.into(), &[dispute]).unwrap();
        assert_eq!(rechecked(), [NodeId::from(window)]);
        merge(rested, kept);
        assert_eq!(
            rechecked(),
            [NodeId::from(window)],
            "a contradicted premise merged away"
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
        let wrong = Link::FlawState {
            flaw: stale,
            state: window,
        };
        store.link(&actor, &[wrong]).unwrap();
        assert_eq!(chain(), [NodeId::from(stale)]);
        let (_, job, _) = begun(&store, &Begin::Chain(vec![stale]));
        end_job(&store, job);
        assert!(chain().is_empty(), "its chain began at its basis");
        let read = access(later, window, Access::Read, at);
        store.link(&actor, &[read]).unwrap();
        check(&store, window, &[basis]);
        assert_eq!(chain(), [NodeId::from(stale)], "a reader recorded since");
        judge(&store, stale, FlawStatus::Refuted);
        assert!(chain().is_empty(), "a refuted flaw");
        let replaced = flaw(&store, &actor, consumer);
        let (questioner, job, _) = begun(&store, &Begin::Chain(vec![replaced]));
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

    /// A path reading an input in code is swept by reading it. One reading none is probed: one
    /// reading only from a binary under any policy, one reading on an interface only once an
    /// environment for any candidate serves it and the run does live work; until then it is
    /// listed uncovered, and holds back no chain. A new environment for any candidate gives an
    /// unswept probe fresh tries when it serves an interface the probe reads recorded input
    /// states on, or the probe reads only binaries, but owes a swept one nothing; one recorded
    /// within a probe offers no other probe again.
    #[test]
    fn a_path_without_code_is_probed_once_its_interfaces_are_served() {
        let uncovered = |store: &Store| {
            (store.synthesis(&[]).unwrap().uncovered.into_iter())
                .map(|uncovered| (uncovered.interface.to_string(), uncovered.paths))
                .collect::<Vec<_>>()
        };
        let (_session, store, actor) = fixture();
        let gateway = component(&store, &actor, "gateway");
        let api = || path_from(&store, &actor, "api", &[gateway], operation("gateway-api"));
        let (code, interface, mixed) = (path(&store, &actor, "code", &[gateway]), api(), api());
        let binary = path_from(&store, &actor, "daemon", &[gateway], daemon(0x4012a0));
        let NodeView::Path(view) = store.node(code.into()).unwrap() else {
            panic!("a path")
        };
        let read = &view.states[0];
        let link = access(mixed, read.id, read.access, read.location.clone());
        store.link(&actor, &[link]).unwrap();
        let owed = |phase| queued(&store, phase);
        assert_eq!(
            owed(Phase::Sweep),
            [NodeId::from(code), mixed.into()],
            "code wins"
        );
        assert_eq!(owed(Phase::Probe), [NodeId::from(binary)]);
        assert_eq!(uncovered(&store), [("gateway-api".to_owned(), 1)]);

        let loose = flaw(&store, &actor, gateway);
        for swept in [code, mixed, binary] {
            sweep(&store, swept);
        }
        assert_eq!(
            owed(Phase::Chain),
            [NodeId::from(loose)],
            "an uncovered probe holds back no chain"
        );
        serve(&store, &actor, &["gateway-api"]).unwrap();
        assert_eq!(owed(Phase::Probe), [NodeId::from(interface)]);
        assert!(uncovered(&store).is_empty());
        assert!(owed(Phase::Chain).is_empty(), "a covered probe comes first");
        sweep(&store, interface);

        component(&store, &actor, "admin");
        let binary = path_from(&store, &actor, "service", &[gateway], daemon(0x4013a0));
        let api = |name| path_from(&store, &actor, name, &[gateway], operation("gateway-api"));
        let (exhausted, probing, swept) = (api("upload"), api("download"), api("list"));
        let admin = New {
            title: text("admin"),
            body: text("an entry recorded in error"),
            detail: NewState::Input {
                location: operation("admin-api"),
                trust: Trust::Untrusted,
            },
        };
        let admin: StateId = (store.upsert(&actor, &Upsert::State(Change::Create(admin))))
            .unwrap()
            .get()
            .try_into()
            .unwrap();
        let link = access(exhausted, admin, Access::Read, operation("admin-api"));
        store.link(&actor, &[link]).unwrap();
        let retracted = StateMark::Inventory(InventoryStatus::Retracted);
        let retracted = StatusChange::State(admin, retracted);
        store
            .set_status(&actor, retracted, &text("in error"))
            .unwrap();
        serve(&store, &actor, &["gateway-api"]).unwrap();
        sweep(&store, swept);
        exhaust(&store, &Begin::Probe(vec![binary]));
        exhaust(&store, &Begin::Probe(vec![exhausted]));
        let (prober, _, _) = begun(&store, &Begin::Probe(vec![probing]));
        serve(&store, &prober, &["gateway-api"]).unwrap();
        assert!(
            queued(&store, Phase::Probe).is_empty(),
            "recorded within a probe"
        );
        serve(&store, &actor, &["admin-api"]).unwrap();
        assert_eq!(queued(&store, Phase::Probe), [NodeId::from(binary)]);
        serve(&store, &actor, &["gateway-api"]).unwrap();
        assert_eq!(
            queued(&store, Phase::Probe),
            [NodeId::from(binary), exhausted.into()]
        );

        let (_unlive, store, actor) = fixture_under(LivePolicy::No);
        let gateway = component(&store, &actor, "gateway");
        path_from(&store, &actor, "api", &[gateway], operation("gateway-api"));
        let binary = path_from(&store, &actor, "daemon", &[gateway], daemon(0x4012a0));
        serve(&store, &actor, &["gateway-api"]).unwrap();
        assert_eq!(
            queued(&store, Phase::Probe),
            [NodeId::from(binary)],
            "no live work, but a binary is read under any policy"
        );
        assert_eq!(uncovered(&store), [("gateway-api".to_owned(), 1)]);
    }
}

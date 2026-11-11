//! Inventory work: paths owing a sweep or a probe, and states owing a check across calls.

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
            $crate::store::work::scope::unscoped!($p),
            " AND EXISTS (SELECT 1 FROM ",
            $crate::store::ledger::facts::driven_reads!("us", $p),
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
            $crate::store::ledger::facts::driven_reads!("xs", $p),
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
            $crate::store::ledger::facts::driven_reads!("xs", $p),
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
            $crate::store::ledger::bearing::basis!("check", $s),
            ">",
            $crate::store::ledger::standing::stamp!("check", $event, $s)
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
            $crate::store::work::scope::state_outside!("co.node"),
            ")"
        )
    };
}
pub(super) use check_owed;

/// State `$s`'s check is owed and not exhausted.
macro_rules! check_pending {
    ($s:expr) => {
        pending!("check", $s, check_owed!($s))
    };
}
pub(super) use check_pending;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::{
        HIGH, access, assess, begun, check, component, daemon, decide, end_job, fixture,
        fixture_under, flaw, location, operation, path, path_from, premise, restep, scenario,
        serve, state, sweep, text, validated, verdict,
    };
    use crate::store::work::queues::tests::{exhaust, queued, status};

    /// A path is owed one sweep: two tasks leaving it unswept exhaust it, with their child's
    /// reply, until it is marked unswept or its path changes; once swept, a later correction
    /// owes nothing. An unswept mark leaves its candidates' settled work settled. A duplicate
    /// path whose input read is moved to the path it repeats owes nothing.
    #[test]
    fn a_path_is_swept_once() {
        let (_session, store, actor) = fixture();
        let scenario = validated(&store, &actor, "decoder");
        let route = NodeId::from(scenario.path);
        let sweep_work = Begin::Sweep(Listed::of([scenario.path]));
        exhaust(&store, &sweep_work);
        let unswept = StatusChange::Path {
            id: scenario.path,
            status: PathStatus::Unswept,
        };
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
            let steps = [(scenario.component, "calls"), (scenario.component, note)];
            restep(&store, &actor, scenario.path, &steps);
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
        let unlink = Edge::PathState {
            path: duplicate,
            state: read.id,
            access: read.access,
            location: read.location.clone(),
        };
        store.relink(&actor, &[], &[unlink]).unwrap();
        let read = access(scenario.path, read.id, read.access, read.location.clone());
        store.relink(&actor, &[read], &[]).unwrap();
        assert!(
            queued(&store, Phase::Sweep).is_empty(),
            "a path reading no input owes no sweep"
        );
        let at = location("decoder again/net.c", 2);
        let [inner, handle] =
            [NewState::Instance, NewState::Handed].map(|scope| state(&store, &actor, scope));
        store
            .relink(
                &actor,
                &[access(duplicate, inner, Access::Read, at.clone())],
                &[],
            )
            .unwrap();
        assert!(
            queued(&store, Phase::Sweep).is_empty(),
            "a path reading only its instance's state owes no sweep"
        );
        store
            .relink(&actor, &[access(duplicate, handle, Access::Read, at)], &[])
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
        let relies = [code, deployment].map(|premise| Edge::PathPremise {
            path: scenario.path,
            premise,
        });
        store.relink(&actor, &relies, &[]).unwrap();
        exhaust(&store, &Begin::Sweep(Listed::of([scenario.path])));
        assess(&store, scenario.candidate, HIGH);
        let relied = || {
            let asked = store.synthesis(&[]).unwrap().asked;
            let review = queued(&store, Phase::Review);
            (queued(&store, Phase::Settle), asked.len(), review)
        };
        assert_eq!(relied(), (vec![NodeId::from(code)], 1, vec![]));
        let mark = |status| {
            let change = StatusChange::Path {
                id: scenario.path,
                status,
            };
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
        store.relink(&actor, &write(3), &[]).unwrap();
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
        let (checker, job, _) = begun(&store, &Begin::Check(Listed::of([window])));
        store.relink(&checker, &write(4), &[]).unwrap();
        let basis = premise(&store, &checker, PremiseScope::Code);
        let checked = StateMark::Checked {
            id: window,
            rests_on: Some(vec![basis]).try_into().unwrap(),
        };
        store
            .set_status(
                &checker,
                StatusChange::State(checked),
                &text("every writer meets every reader"),
            )
            .unwrap();
        end_job(&store, job);
        assert!(check().is_empty(), "its own check's accesses");
        store.relink(&actor, &write(5), &[]).unwrap();
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
        store.relink(&actor, &[write], &[]).unwrap();
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
        store.relink(&actor, &[write], &[]).unwrap();
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
        store.relink(&actor, &[link], &[]).unwrap();
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
        let admin = NewState::Input(NewInput {
            title: text("admin"),
            body: text("an entry recorded in error"),
            location: operation("admin-api"),
            trust: Trust::Untrusted,
        });
        let admin: StateId = (store.upsert(&actor, &Upsert::Create(NewNode::State(admin))))
            .unwrap()
            .0
            .try_into()
            .unwrap();
        let link = access(exhausted, admin, Access::Read, operation("admin-api"));
        store.relink(&actor, &[link], &[]).unwrap();
        let retracted = StatusChange::State(StateMark::Retracted { id: admin });
        store
            .set_status(&actor, retracted, &text("in error"))
            .unwrap();
        serve(&store, &actor, &["gateway-api"]).unwrap();
        sweep(&store, swept);
        exhaust(&store, &Begin::Probe(Listed::of([binary])));
        exhaust(&store, &Begin::Probe(Listed::of([exhausted])));
        let (prober, _, _) = begun(&store, &Begin::Probe(Listed::of([probing])));
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

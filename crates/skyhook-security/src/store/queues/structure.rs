//! Structure work: paths owing a sweep or a probe, premises owing a settling look and flaws
//! owing a chain.

use super::*;
use crate::store::reads::disputed;

/// Path `$p` is unswept and some recorded source enters it: a correction of a swept path owes
/// nothing, and a path left without sources, or only retracted ones, is no flow to sweep.
macro_rules! unswept {
    ($p:expr) => {
        concat!(
            "(SELECT status='unswept' FROM path WHERE node=",
            $p,
            ") AND EXISTS (SELECT 1 FROM path_source us JOIN source uo ON uo.node=us.source WHERE \
             us.path=",
            $p,
            " AND uo.status='recorded')"
        )
    };
}
pub(super) use unswept;

/// No recorded source entering path `$p` sits in code: its sweep is a probe, which observes or
/// disassembles what it cannot read.
macro_rules! probed {
    ($p:expr) => {
        concat!(
            "NOT EXISTS (SELECT 1 FROM path_source xs JOIN source xo ON xo.node=xs.source JOIN \
             code_location xc ON xc.location=xo.location WHERE xs.path=",
            $p,
            " AND xo.status='recorded')"
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

/// A recorded source entering path `$p` sits on an interface that is not served.
macro_rules! uncovered {
    ($p:expr) => {
        concat!(
            "EXISTS (SELECT 1 FROM path_source xs JOIN source xo ON xo.node=xs.source JOIN \
             interface_location xi ON xi.location=xo.location WHERE xs.path=",
            $p,
            " AND xo.status='recorded' AND NOT ",
            served!("xi.interface"),
            ")"
        )
    };
}
pub(super) use uncovered;

/// Path `$p` owes a sweep: it is unswept and a source entering it sits in code.
macro_rules! sweep_owed {
    ($p:expr) => {
        concat!("(", unswept!($p), " AND NOT ", probed!($p), ")")
    };
}
pub(super) use sweep_owed;

/// Path `$p` owes a probe: it is unswept, no source entering it sits in code, and every
/// interface its sources sit on is served.
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

/// Path `$p`'s sweep or probe is owed and not exhausted.
macro_rules! structure_pending {
    ($p:expr) => {
        concat!(
            "(",
            pending!("sweep", $p, sweep_owed!($p)),
            " OR ",
            pending!("probe", $p, probe_owed!($p)),
            ")"
        )
    };
}
pub(in crate::store) use structure_pending;

/// Candidate `$c` is live: unmerged, and not judged disproved at its current review. An
/// excluded candidate still holds its flaws.
macro_rules! live_candidate {
    ($c:expr) => {
        concat!(
            "EXISTS (SELECT 1 FROM candidate_core lc WHERE lc.candidate=",
            $c,
            " AND lc.merged_into IS NULL AND NOT (lc.reviewed_current AND \
             ifnull(lc.verdict='disproved',0)))"
        )
    };
}
pub(super) use live_candidate;

/// Flaw `$f` is live: unmerged and suspected, on no unmerged candidate or on a live one.
macro_rules! live_flaw {
    ($f:expr) => {
        concat!(
            "(EXISTS (SELECT 1 FROM flaw_state lf JOIN node lfn ON lfn.id=lf.node WHERE lf.node=",
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

/// Premise `$p` owes a settling look: it is about the code, undecided or disputed since its
/// decision, and a live flaw depends on it or an unretracted path relies on it. A deployment
/// premise is the user's to answer.
macro_rules! settle_owed {
    ($p:expr) => {
        concat!(
            "(EXISTS (SELECT 1 FROM premise_state so WHERE so.node=",
            $p,
            " AND so.scope='code' AND (so.status='undecided' OR ",
            disputed!("so.node", "so.decided"),
            ")) AND (EXISTS (SELECT 1 FROM flaw_premise sf WHERE sf.premise=",
            $p,
            " AND ",
            live_flaw!("sf.flaw"),
            ") OR EXISTS (SELECT 1 FROM path_premise sp JOIN path sq ON sq.node=sp.path AND \
             sq.status!='retracted' WHERE sp.premise=",
            $p,
            ")))"
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

/// Flaw `$f` owes a chain: it is on no live candidate, and suspected, refuted but disputed
/// since, or confirmed with something it rests on changed since.
macro_rules! chain_owed {
    ($f:expr) => {
        concat!(
            "(EXISTS (SELECT 1 FROM flaw_state co WHERE co.node=",
            $f,
            " AND CASE co.status WHEN 'suspected' THEN 1 WHEN 'refuted' THEN ",
            disputed!("co.node", "co.judged"),
            " ELSE ifnull(",
            basis!("chain", "co.node"),
            ">",
            $crate::store::bearing::stamp!("chain", "co.judged", "co.node"),
            ",0) END) AND NOT EXISTS (SELECT 1 FROM candidate_flaw cc WHERE cc.flaw=",
            $f,
            " AND ",
            live_candidate!("cc.candidate"),
            "))"
        )
    };
}

pub(super) const SWEEP: Sql = phase_sql!("sweep", "path", sweep_owed!("n.id"), "1", "NULL", "NULL");

pub(super) const PROBE: Sql = phase_sql!("probe", "path", probe_owed!("n.id"), "1", "NULL", "NULL");

/// Bundled: premises sharing a dependent flaw go to one agent.
pub(super) const SETTLE: Sql = phase_sql!(
    "settle",
    "premise",
    settle_owed!("n.id"),
    "1",
    "NULL",
    "SELECT value FROM json_each(?1) UNION SELECT premise FROM flaw_premise WHERE flaw IN \
     (SELECT value FROM json_each(?1))"
);

/// Ready once no path owes a sweep or a probe: what a flaw leaves may be read on any path.
pub(super) const CHAIN: Sql = phase_sql!(
    "chain",
    "flaw",
    chain_owed!("n.id"),
    concat!(
        "NOT EXISTS (SELECT 1 FROM path rp WHERE ",
        structure_pending!("rp.node"),
        ")"
    ),
    "NULL",
    "SELECT value FROM json_each(?1)"
);

#[cfg(test)]
mod tests {
    use super::{
        super::{
            super::tests::{
                HIGH, assess, begun, candidate, component, daemon, end_job, fixture, fixture_under,
                flaw, last_event, marks, operation, path, path_from, premise, scenario, serve,
                sweep, text, validated, verdict,
            },
            tests::{exhaust, queued, status},
        },
        *,
    };

    /// A path is owed one sweep: two tasks leaving it unswept exhaust it, with their child's
    /// reply, until it is marked unswept or its path changes; once swept, a later correction
    /// owes nothing. An unswept mark leaves its candidates' settled work settled. A duplicate
    /// path whose source is relinked to the path it repeats owes nothing.
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
        let source = view.sources[0].id;
        let unlink = Unlink::PathSource {
            path: duplicate,
            source,
        };
        store.unlink(&actor, &[unlink]).unwrap();
        let link = Link::PathSource {
            path: scenario.path,
            source,
        };
        store.link(&actor, &[link]).unwrap();
        assert!(
            queued(&store, Phase::Sweep).is_empty(),
            "a path left without sources owes no sweep"
        );
    }

    /// A retracted path owes no sweep, exhausted or not, and counts nowhere, the component only
    /// it reaches unreached; the premises it relies on owe no settling look, ask nothing of the
    /// user and hold back no review of the candidate performing it. Marked unswept again, it is
    /// offered its sweep afresh.
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
        assess(&store, &actor, scenario.candidate, HIGH);
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
        mark(PathStatus::Unswept);
        assert_eq!(queued(&store, Phase::Sweep), [NodeId::from(scenario.path)]);
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
        store
            .set_status(
                &actor,
                StatusChange::Flaw(lone, FlawStatus::Refuted),
                &text("bounded"),
            )
            .unwrap();
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

        let decided = StatusChange::Premise(code, PremiseStatus::Holds);
        store.set_status(&actor, decided, &text("checked")).unwrap();
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
        let merge = Merge::Premise {
            premise: premise(&store, &actor, PremiseScope::Code),
            into: code,
            note: text("the same check"),
        };
        store.merge(&settler, &merge).unwrap();
        assert_eq!(settle(), [NodeId::from(code), loose.into()], "a merge");
        store.set_status(&actor, decided, &text("checked")).unwrap();
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

        assess(&store, &actor, scenario.candidate, HIGH);
        let asked = store.synthesis(&[]).unwrap().asked;
        assert_eq!(
            asked.iter().map(|asked| asked.premise).collect::<Vec<_>>(),
            [deployment]
        );
    }

    /// A flaw is owed a chain while no live candidate holds it, once every path's sweep ends,
    /// a path elsewhere included; a flaw only a disproved candidate holds, or one leaving a
    /// chain, is owed again with fresh tries, one an excluded candidate holds is not, and a
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
        assert!(chain().is_empty(), "a path elsewhere is unswept");
        sweep(&store, consumer);
        assert_eq!(chain(), [NodeId::from(loose)]);
        let held = candidate(&store, &actor, scenario.path, &[loose]);
        assert!(chain().is_empty());
        assess(&store, &actor, held, HIGH);
        verdict(&store, &actor, held, Verdict::Excluded);
        assert!(chain().is_empty(), "an excluded candidate holds its flaw");
        verdict(&store, &actor, held, Verdict::Disproved);
        assert_eq!(
            chain(),
            [NodeId::from(loose)],
            "a disproved candidate holds nothing"
        );

        let apart = flaw(&store, &actor, scenario.component);
        let confirmed = StatusChange::Flaw(apart, FlawStatus::Confirmed);
        let (questioner, job, _) = begun(&store, &Begin::Chain(vec![apart]));
        store
            .set_status(&questioner, confirmed, &text("no consequence"))
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

        let judge = |flaw, status| {
            let change = StatusChange::Flaw(flaw, status);
            store.set_status(&actor, change, &text("judged")).unwrap();
        };
        let decide = |premise| {
            let change = StatusChange::Premise(premise, PremiseStatus::Holds);
            store.set_status(&actor, change, &text("traced")).unwrap();
        };
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
        assess(&store, &actor, behind, HIGH);
        verdict(&store, &actor, behind, Verdict::Disproved);
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

    /// A chain that replaces its flaw with a new one merged into it owes the new one a chain,
    /// resting on the merge.
    #[test]
    fn a_flaw_replaced_within_its_chain_is_owed_one() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        sweep(&store, scenario.path);
        let replaced = flaw(&store, &actor, scenario.component);
        let (questioner, job, _) = begun(&store, &Begin::Chain(vec![replaced]));
        let replacement = flaw(&store, &questioner, scenario.component);
        let merge = Merge::Flaw {
            flaw: replaced,
            into: replacement,
            note: text("restated"),
        };
        store.merge(&questioner, &merge).unwrap();
        end_job(&store, job);
        assert_eq!(queued(&store, Phase::Chain), [NodeId::from(replacement)]);
        assert_eq!(store.run_facts().unwrap().summary.owed(Phase::Chain), 1);
    }

    /// A path with a source in code is swept by reading it. One with none is probed: one entered
    /// only from a binary under any policy, one entered on an interface only once an
    /// environment for any candidate serves it and the run does live work; until then it is
    /// listed uncovered, and holds back no chain.
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
        let link = Link::PathSource {
            path: mixed,
            source: view.sources[0].id,
        };
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

    /// A new environment for any candidate gives an unswept probe fresh tries when it serves
    /// an interface the probe's recorded sources sit on, or the probe enters only from binaries,
    /// but owes a swept one nothing; one recorded within a probe offers no other probe again.
    #[test]
    fn a_new_environment_offers_only_the_unswept_probes_it_serves_again() {
        let (_session, store, actor) = fixture();
        let gateway = component(&store, &actor, "gateway");
        component(&store, &actor, "admin");
        let binary = path_from(&store, &actor, "daemon", &[gateway], daemon(0x4012a0));
        let api = |name| path_from(&store, &actor, name, &[gateway], operation("gateway-api"));
        let (exhausted, probing, swept) = (api("upload"), api("download"), api("list"));
        let admin = New {
            title: text("admin"),
            body: text("an entry recorded in error"),
            detail: NewSource {
                location: operation("admin-api"),
                trust: Trust::Untrusted,
            },
        };
        let admin: SourceId = (store.upsert(&actor, &Upsert::Source(Change::Create(admin))))
            .unwrap()
            .get()
            .try_into()
            .unwrap();
        let link = Link::PathSource {
            path: exhausted,
            source: admin,
        };
        store.link(&actor, &[link]).unwrap();
        let retracted = StatusChange::Source(admin, InventoryStatus::Retracted);
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
    }
}

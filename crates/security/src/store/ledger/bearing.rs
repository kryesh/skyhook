//! What each phase's work on a node reads: the events that move it, whose latest is the basis
//! and whose later ones are notices. Each relation is spelled with the node in every arm,
//! because SQLite pushes no correlated term into a compound view: read for each subject of a
//! listing, a view would scan its tables whole for each.

use std::collections::BTreeMap;

use skyhook::db::{Db, params};

use crate::store::*;

/// How an event of `kind` naming a node with `effect` bears on `phase`'s work on that node, if
/// it does, by the arms of the phase's bearing that read only the event's own effects: what
/// [`record`] stores. Work rests on its subject's standing and changed effects: a check on its
/// state's accesses, edits and merges, a report on its root's own standing changes, which a
/// candidate taking or losing it as its root makes. A test rests on the assessment and its
/// candidate's test records, which name it alone and bear on nothing else, and is edited by what
/// it demonstrates changing: a verdict of another value, or the candidate's chain or actions,
/// which evidence moves only on a premise of its chain, carried as standing; evidence on the
/// candidate, its flaws or its paths changes neither. The proposals disputing a settle's,
/// chain's or review's result give its work fresh tries.
fn role(phase: Phase, kind: EventKind, effect: EventEffect) -> Option<BearingRole> {
    use {BearingRole as R, EventEffect as E, EventKind as K, Phase as P};
    match (phase, kind, effect) {
        (P::Test, K::Tested, _) => Some(R::Rests),
        (_, K::Tested, _) => None,
        (P::Settle | P::Chain, K::Status, E::Disputes)
        | (P::Review, K::Assessed | K::Verdict, E::Disputes) => Some(R::Disputes),
        (_, _, E::Disputes) => None,
        (P::Test, K::Assessed, E::Standing) => Some(R::Rests),
        (P::Test, K::Evidence, E::Changed) => None,
        (P::Test, _, E::Standing | E::Changed) => Some(R::Edits),
        // A chain also rests on the chain edits it joins or leaves.
        (P::Chain, K::Edited, E::Concerns) => Some(R::Rests),
        (_, K::Edited | K::Merged, E::Changed) => Some(R::Edits),
        (_, _, E::Standing | E::Changed) => Some(R::Rests),
        (_, _, E::Concerns) => None,
    }
}

/// Record the work event `event` of `kind` was recorded within, before its write: a merge is
/// within none.
pub(in crate::store) fn record_work(db: &Db, event: EventId, kind: EventKind) -> Result<()> {
    if kind != EventKind::Merged {
        db.execute(
            "WITH held(phase,node,kind,basis) AS (SELECT wn.phase,wn.node,wn.kind,wn.basis FROM \
             event_task wt CROSS JOIN task_node wn ON wn.task=wt.task WHERE wt.event=?1 UNION ALL \
             SELECT wn.phase,wm.survivor,wn.kind,wn.basis FROM event_task wt CROSS JOIN task_node \
             wn ON wn.task=wt.task JOIN node_merge wm ON wm.node=wn.node AND wm.event=(SELECT \
             max(wr.event) FROM node_merge wr WHERE wr.node=wn.node AND wr.event<?1) WHERE \
             wt.event=?1) INSERT INTO event_work(event,phase,node,kind,basis) SELECT \
             ?1,phase,node,kind,max(basis) FROM held GROUP BY phase,node",
            params![event],
        )?;
    }
    Ok(())
}

/// Record the events of each phase's bearing that event `event` of `kind`, naming nodes with
/// `marks`, is by its own effects ([`role`]), as `event_bearing` rows, but those of a phase's
/// work on a node the event was recorded within: a phase's own work never moves what it rests
/// on. A pending test is resumed rather than repeated, so its record moves the test's basis even
/// within its own work.
pub(in crate::store) fn record(
    db: &Db,
    event: EventId,
    kind: EventKind,
    marks: &BTreeMap<NodeId, EventEffect>,
) -> Result<()> {
    let roles: Vec<(Phase, NodeId, BearingRole)> = (marks.iter())
        .flat_map(|(&node, &effect)| {
            (Phase::ALL.into_iter())
                .filter_map(move |phase| Some((phase, node, role(phase, kind, effect)?)))
        })
        .collect();
    if !roles.is_empty() {
        db.execute(
            "INSERT INTO event_bearing(node,phase,event,kind,role) SELECT n.id,p.name,?1,n.kind,\
             j.value->>2 FROM json_each(?2) j JOIN task_phase p ON p.name=j.value->>0 JOIN node n \
             ON n.id=j.value->>1 AND n.kind=p.subject WHERE NOT EXISTS (SELECT 1 FROM event_work \
             w WHERE w.event=?1 AND w.phase=p.name AND w.node=n.id) OR EXISTS (SELECT 1 FROM test \
             t WHERE t.event=?1 AND t.candidate=n.id AND t.result='pending')",
            params![event, json_list(&roles)?],
        )?;
    }
    Ok(())
}

/// The events of `$phase`'s bearing on node `$n`, but those recorded within a task of
/// `$phase` on it, by its agent or one beneath it: a phase's own work never moves what it rests
/// on. The arms reading only an event's own effects are stored as it is recorded
/// (`event_bearing`, [`role`]); the arms reading relations a later event may change are read
/// here, each a list of arms: `each` events bearing on every node a query selects as `node`,
/// `tests` the test records of each such node, and `environment` environments recorded. Test
/// records bear on their candidate through `tests` alone, never `each`, which reads a node's
/// effects down an index holding none of them. Notices read the arms whole as this stream; the
/// [`basis`] reads each newest first down its index and stops at its first event that counts,
/// past none older.
macro_rules! bearing {
    (@arms "sweep", $n:expr, $then:ident) => {
        $crate::store::ledger::bearing::bearing!(@$then "sweep", $n)
    };
    // A probe also rests on the environments for any candidate serving an interface its reads of
    // recorded caller-supplied states sit on, or any such one when they sit on none, but not one recorded
    // within a probe: a new one gives an unfinished probe fresh tries.
    (@arms "probe", $n:expr, $then:ident) => {
        $crate::store::ledger::bearing::bearing!(@$then "probe", $n;
            environment concat!(
                "ev.candidate IS NULL AND NOT EXISTS (SELECT 1 FROM event_work pw WHERE \
                 pw.event=ev.event AND pw.phase='probe') AND coalesce((SELECT \
                 max(EXISTS (SELECT 1 FROM environment_interface ei WHERE ei.environment=ev.id AND \
                 ei.interface=pi.interface)) FROM ",
                $crate::store::ledger::facts::driven_reads!("ps", $n),
                " JOIN interface_location pi ON pi.location=ps.location),1)"
            ))
    };
    // A check also rests on the premises its clearing relies on, each with the premises merged
    // into it, whose evidence it carries: one decided failing, or contradicting evidence on one,
    // owes it again; one decided holding, or a merge itself, does not.
    (@arms "check", $n:expr, $then:ident) => {
        $crate::store::ledger::bearing::bearing!(@$then "check", $n;
            each concat!(
                "SELECT premise AS node FROM state_premise WHERE state=",
                $n,
                " UNION ALL SELECT pm.id FROM state_premise sp JOIN node pm ON \
                 pm.merged_into=sp.premise WHERE sp.state=",
                $n
            ),
                "en.effect='changed' AND (en.kind='evidence' OR EXISTS (SELECT 1 FROM \
                 premise_decision ef WHERE ef.event=en.event AND ef.premise=en.node AND \
                 ef.status='fails'))")
    };
    // A settle also rests on the decisions and disputes of the premises it rests on.
    (@arms "settle", $n:expr, $then:ident) => {
        $crate::store::ledger::bearing::bearing!(@$then "settle", $n;
            each $crate::store::ledger::bearing::bearing!(@rests $n), $crate::store::ledger::bearing::bearing!(@rests))
    };
    (@rests $n:expr) => {
        concat!("SELECT rests_on AS node FROM premise_basis WHERE premise=", $n)
    };
    (@rests) => {
        "en.effect='changed' AND en.kind IN ('status','evidence')"
    };
    // A chain also rests on the verdicts of the candidates holding its flaw, a disproval or a
    // leave releasing it with fresh tries, and on the accesses recorded or removed of the states
    // it leaves wrong.
    (@arms "chain", $n:expr, $then:ident) => {
        $crate::store::ledger::bearing::bearing!(@$then "chain", $n;
            each concat!("SELECT candidate AS node FROM candidate_flaw WHERE flaw=", $n),
                "en.effect='standing' AND en.kind='verdict'";
            each concat!("SELECT state AS node FROM flaw_state WHERE flaw=", $n),
                $crate::store::ledger::bearing::bearing!(@accesses))
    };
    // A review also rests on the accesses recorded or removed of the states its flaws touch:
    // those they leave wrong, and those its paths read where one sits, in its function, or at its
    // location when it names none. An access of any other state on its paths does not bear on it.
    (@arms "review", $n:expr, $then:ident) => {
        $crate::store::ledger::bearing::bearing!(@$then "review", $n;
            each concat!(
                "SELECT fs.state AS node FROM candidate_flaw rf JOIN flaw_state fs ON \
                 fs.flaw=rf.flaw WHERE rf.candidate=",
                $n,
                " UNION SELECT ps.state FROM candidate_flaw rf JOIN flaw rl ON rl.node=rf.flaw \
                 JOIN candidate_action ra ON ra.candidate=rf.candidate JOIN path_state ps ON \
                 ps.path=ra.path AND ps.access='read' LEFT JOIN code_location fc ON \
                 fc.location=rl.location LEFT JOIN code_location pc ON pc.location=ps.location \
                 WHERE rf.candidate=",
                $n,
                " AND (ps.location=rl.location OR (pc.file=fc.file AND pc.function=fc.function))"
            ),
                $crate::store::ledger::bearing::bearing!(@accesses))
    };
    (@accesses) => {
        "en.effect='standing' AND en.kind IN ('linked','unlinked')"
    };
    // A test also rests on the environments for its candidate, which a merge into it moves to
    // it, or for any.
    (@arms "test", $n:expr, $then:ident) => {
        $crate::store::ledger::bearing::bearing!(@$then "test", $n;
            environment concat!("ev.candidate=", $n);
            environment "ev.candidate IS NULL")
    };
    // A report also rests on every current member's standing changes, a settled test's
    // included. The begins of its tasks offered other reportable members than its group has now
    // give it fresh tries, as disputes do, however the members changed, a restated result
    // included; they are no notices.
    (@arms "report", $n:expr, $then:ident) => {
        $crate::store::ledger::bearing::bearing!(@$then "report", $n;
            each $crate::store::ledger::bearing::bearing!(@members $n), "en.effect='standing'";
            each $crate::store::ledger::bearing::bearing!(@members $n), "en.effect='changed'";
            tests $crate::store::ledger::bearing::bearing!(@members $n), "t.result!='pending'";
            offers)
    };
    (@members $n:expr) => {
        concat!(
            "SELECT r.candidate AS node FROM candidate_root r JOIN node rn ON rn.id=r.candidate \
             WHERE rn.merged_into IS NULL AND r.root=",
            $n
        )
    };
    (@stored $phase:tt, $n:expr) => {
        concat!("event_bearing eb WHERE eb.node=", $n, " AND eb.phase='", $phase, "'")
    };
    (@stream $phase:tt, $n:expr $(; $kinds:ident $($as:expr),*)*) => {
        concat!(
            "SELECT ob.event FROM (SELECT eb.event FROM ",
            $crate::store::ledger::bearing::bearing!(@stored $phase, $n),
            $($crate::store::ledger::bearing::bearing!(@union $phase, $n; $kinds $($as),*),)*
            ") ob"
        )
    };
    (@union $phase:tt, $n:expr; offers) => {
        ""
    };
    (@union $phase:tt, $n:expr; $kind:ident $($as:expr),*) => {
        concat!(" UNION ALL ", $crate::store::ledger::bearing::bearing!(@arm $phase, $n; $kind $($as),*))
    };
    (@arm $phase:tt, $n:expr; each $members:expr, $cond:expr) => {
        concat!(
            "SELECT en.event FROM (",
            $members,
            ") m JOIN event_node en ON en.node=m.node WHERE en.kind!='tested' AND ",
            $cond,
            " AND NOT ",
            $crate::store::ledger::standing::owns!($phase, "en.event", $n)
        )
    };
    (@arm $phase:tt, $n:expr; tests $members:expr, $cond:expr) => {
        concat!(
            "SELECT t.event FROM (",
            $members,
            ") m JOIN test t ON t.candidate=m.node WHERE ",
            $cond,
            " AND NOT ",
            $crate::store::ledger::standing::owns!($phase, "t.event", $n)
        )
    };
    (@arm $phase:tt, $n:expr; environment $cond:expr) => {
        concat!(
            "SELECT ev.event FROM environment ev WHERE ",
            $cond,
            " AND NOT ",
            $crate::store::ledger::standing::owns!($phase, "ev.event", $n)
        )
    };
    (@latest $phase:tt, $n:expr $(; $kinds:ident $($as:expr),*)*) => {
        $crate::store::ledger::bearing::bearing!(@max last,
            concat!("SELECT max(eb.event) FROM ", $crate::store::ledger::bearing::bearing!(@stored $phase, $n)),
            $phase, $n $(; $kinds $($as),*)*)
    };
    // What a standing result is current against: the bearing but its disputes, and the offers
    // that, like them, give its work fresh tries.
    (@rested $phase:tt, $n:expr $(; $kinds:ident $($as:expr),*)*) => {
        $crate::store::ledger::bearing::bearing!(@max rest,
            concat!(
                "SELECT eb.event FROM ",
                $crate::store::ledger::bearing::bearing!(@stored $phase, $n),
                " AND eb.role!='disputes' ORDER BY eb.event DESC LIMIT 1"
            ),
            $phase, $n $(; $kinds $($as),*)*)
    };
    (@max $last:ident, $stored:expr, $phase:tt, $n:expr) => {
        concat!("(", $stored, ")")
    };
    (@max $last:ident, $stored:expr, $phase:tt, $n:expr $(; $kinds:ident $($as:expr),*)+) => {
        concat!(
            "(SELECT max(event) FROM (SELECT (",
            $stored,
            ") AS event",
            $(" UNION ALL SELECT (",
                $crate::store::ledger::bearing::bearing!(@$last $phase, $n; $kinds $($as),*), ")",)+
            "))"
        )
    };
    (@rest $phase:tt, $n:expr; offers) => {
        "NULL"
    };
    (@rest $phase:tt, $n:expr; $kind:ident $($as:expr),*) => {
        $crate::store::ledger::bearing::bearing!(@last $phase, $n; $kind $($as),*)
    };
    (@last $phase:tt, $n:expr; each $members:expr, $cond:expr) => {
        concat!(
            "SELECT max((SELECT en.event FROM event_node en WHERE en.node=m.node AND \
             en.kind!='tested' AND ",
            $cond,
            " AND NOT ",
            $crate::store::ledger::standing::owns!($phase, "en.event", $n),
            " ORDER BY en.event DESC LIMIT 1)) FROM (",
            $members,
            ") m"
        )
    };
    (@last $phase:tt, $n:expr; offers) => {
        concat!(
            "SELECT max(ot.began) FROM task_node otn JOIN task ot ON ot.id=otn.task WHERE \
             otn.phase='",
            $phase,
            "' AND otn.node=",
            $n,
            " AND NOT ",
            $crate::store::ledger::bearing::reported_as!("ot.began", $n)
        )
    };
    // A test's id orders it as its event does.
    (@last $phase:tt, $n:expr; tests $members:expr, $cond:expr) => {
        concat!(
            "SELECT max((SELECT t.event FROM test t WHERE t.candidate=m.node AND ",
            $cond,
            " AND NOT ",
            $crate::store::ledger::standing::owns!($phase, "t.event", $n),
            " ORDER BY t.id DESC LIMIT 1)) FROM (",
            $members,
            ") m"
        )
    };
    (@last $phase:tt, $n:expr; environment $cond:expr) => {
        concat!(
            "SELECT ev.event FROM environment ev WHERE ",
            $cond,
            " AND NOT ",
            $crate::store::ledger::standing::owns!($phase, "ev.event", $n),
            " ORDER BY ev.event DESC LIMIT 1"
        )
    };
    ($phase:tt, $n:expr) => {
        $crate::store::ledger::bearing::bearing!(@arms $phase, $n, stream)
    };
}
pub(in crate::store) use bearing;

/// Root `$r`'s reportable members are those event `$event` of its report work recorded
/// ([`record_scenarios`]).
macro_rules! reported_as {
    ($event:expr, $r:expr) => {
        concat!(
            "(SELECT count(*)=total(x.candidate IN (SELECT candidate FROM report_scenario WHERE \
             event=",
            $event,
            " AND root=",
            $r,
            ")) AND count(*)=(SELECT count(*) FROM report_scenario WHERE event=",
            $event,
            " AND root=",
            $r,
            ") FROM finding_scenario x WHERE x.root=",
            $r,
            ")"
        )
    };
}
pub(in crate::store) use reported_as;

/// Record root `root`'s reportable members as event `event` of its report work leaves them.
pub(in crate::store) fn record_scenarios(
    db: &Db,
    event: EventId,
    root: impl Into<NodeId>,
) -> Result<()> {
    let root = root.into();
    db.execute(
        "INSERT INTO report_scenario(event,root,candidate) SELECT ?1,root,candidate FROM \
         finding_scenario WHERE root=?2",
        params![event, root],
    )?;
    Ok(())
}

/// The basis of `$phase`'s work on node `$n`: the latest event of its bearing, or the node's
/// latest event while its bearing holds none, as an untested candidate's test, so that work
/// owed always rests on one. `rested` leaves out its `disputes`: what a standing result is
/// current against.
macro_rules! basis {
    ($phase:tt, $n:expr) => {
        $crate::store::ledger::bearing::basis!(latest $phase, $n)
    };
    ($mode:ident $phase:tt, $n:expr) => {
        concat!(
            "coalesce(",
            $crate::store::ledger::bearing::bearing!(@arms $phase, $n, $mode),
            ",(SELECT max(event) FROM event_node WHERE node=",
            $n,
            "))"
        )
    };
}
pub(in crate::store) use basis;

/// The merge retiring node `$n` into another, none while it is live: the first merge retargeting
/// it, as later ones only repoint it.
macro_rules! retirement {
    ($n:expr) => {
        concat!(
            "(SELECT min(rm.event) FROM node_merge rm WHERE rm.node=",
            $n,
            ")"
        )
    };
}
pub(in crate::store) use retirement;

/// Per phase, the live node `?1` names with its kind and what the phase's work on it rests on
/// now.
macro_rules! lookup {
    ($phase:tt) => {
        concat!(
            "SELECT s.id,s.kind,",
            basis!($phase, "s.id"),
            " FROM node n JOIN node s ON s.id=coalesce(n.merged_into,n.id) WHERE n.id=?1"
        )
    };
}

/// The statement looking up a subject of `phase` with its basis.
pub(in crate::store) const fn lookup(phase: Phase) -> &'static str {
    crate::store::work::tasks::per_phase!(phase, lookup)
}

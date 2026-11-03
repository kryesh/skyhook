//! Relations read per node. What each phase's work on a node reads: the events that move it,
//! whose latest is the basis and whose later ones are notices; and the premises a candidate's
//! chain rests on. Each is spelled with the node in every arm, because SQLite pushes no
//! correlated term into a compound view: read for each subject of a listing, a view would scan
//! its tables whole for each. The derived views reading them are generated here too.

use std::collections::BTreeMap;

use skyhook::db::{Db, params};

use super::*;

/// How an event of `kind` naming a node with `effect` bears on `phase`'s work on that node, if
/// it does, by the arms of the phase's bearing that read only the event's own effects: what
/// [`record`] stores. Work rests on its subject's standing and changed effects: a check on its
/// state's accesses, edits and merges, a report on its root's own standing changes, which a
/// candidate taking or losing it as its root makes. An assessment rests on changes alone. A test
/// rests on the assessment and its candidate's test records, which name it alone and bear on
/// nothing else, and is edited by what it demonstrates changing: a verdict of another value, or
/// the candidate's chain or actions, which evidence moves only on a premise of its chain, carried
/// as standing; evidence on the candidate, its flaws or its paths changes neither. The proposals
/// disputing a settle's, chain's, assessment's or review's result give its work fresh tries.
fn role(phase: Phase, kind: EventKind, effect: EventEffect) -> Option<BearingRole> {
    use {BearingRole as R, EventEffect as E, EventKind as K, Phase as P};
    match (phase, kind, effect) {
        (P::Test, K::Tested, _) => Some(R::Rests),
        (_, K::Tested, _) => None,
        (P::Settle | P::Chain, K::Status, E::Disputes)
        | (P::Assess, K::Assessed, E::Disputes)
        | (P::Review, K::Verdict, E::Disputes) => Some(R::Disputes),
        (_, _, E::Disputes) => None,
        (P::Test, K::Assessed, E::Standing) => Some(R::Rests),
        (P::Test, K::Evidence, E::Changed) | (P::Assess, _, E::Standing) => None,
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
pub(super) fn record_work(db: &Db, event: EventId, kind: EventKind) -> Result<()> {
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
pub(super) fn record(
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
        $crate::store::bearing::bearing!(@$then "sweep", $n)
    };
    // A probe also rests on the environments for any candidate serving an interface its reads of
    // recorded caller-supplied states sit on, or any such one when they sit on none, but not one recorded
    // within a probe: a new one gives an unfinished probe fresh tries.
    (@arms "probe", $n:expr, $then:ident) => {
        $crate::store::bearing::bearing!(@$then "probe", $n;
            environment concat!(
                "ev.candidate IS NULL AND NOT EXISTS (SELECT 1 FROM event_work pw WHERE \
                 pw.event=ev.event AND pw.phase='probe') AND coalesce((SELECT \
                 max(EXISTS (SELECT 1 FROM environment_interface ei WHERE ei.environment=ev.id AND \
                 ei.interface=pi.interface)) FROM ",
                $crate::store::bearing::driven_reads!("ps", $n),
                " JOIN interface_location pi ON pi.location=ps.location),1)"
            ))
    };
    (@arms "check", $n:expr, $then:ident) => {
        $crate::store::bearing::bearing!(@$then "check", $n)
    };
    // A settle also rests on the decisions and disputes of the premises it rests on.
    (@arms "settle", $n:expr, $then:ident) => {
        $crate::store::bearing::bearing!(@$then "settle", $n;
            each $crate::store::bearing::bearing!(@rests $n), $crate::store::bearing::bearing!(@rests))
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
        $crate::store::bearing::bearing!(@$then "chain", $n;
            each concat!("SELECT candidate AS node FROM candidate_flaw WHERE flaw=", $n),
                "en.effect='standing' AND en.kind='verdict'";
            each concat!("SELECT state AS node FROM flaw_state WHERE flaw=", $n),
                "en.effect='standing' AND en.kind IN ('linked','unlinked')")
    };
    (@arms "assess", $n:expr, $then:ident) => {
        $crate::store::bearing::bearing!(@$then "assess", $n)
    };
    (@arms "review", $n:expr, $then:ident) => {
        $crate::store::bearing::bearing!(@$then "review", $n)
    };
    // A test also rests on the environments for its candidate, which a merge into it moves to
    // it, or for any.
    (@arms "test", $n:expr, $then:ident) => {
        $crate::store::bearing::bearing!(@$then "test", $n;
            environment concat!("ev.candidate=", $n);
            environment "ev.candidate IS NULL")
    };
    // A report also rests on every current member's standing changes, a settled test's
    // included. The begins of its tasks offered other reportable members than its group has now
    // give it fresh tries, as disputes do, however the members changed, a restated result
    // included; they are no notices.
    (@arms "report", $n:expr, $then:ident) => {
        $crate::store::bearing::bearing!(@$then "report", $n;
            each $crate::store::bearing::bearing!(@members $n), "en.effect='standing'";
            each $crate::store::bearing::bearing!(@members $n), "en.effect='changed'";
            tests $crate::store::bearing::bearing!(@members $n), "t.result!='pending'";
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
            $crate::store::bearing::bearing!(@stored $phase, $n),
            $($crate::store::bearing::bearing!(@union $phase, $n; $kinds $($as),*),)*
            ") ob"
        )
    };
    (@union $phase:tt, $n:expr; offers) => {
        ""
    };
    (@union $phase:tt, $n:expr; $kind:ident $($as:expr),*) => {
        concat!(" UNION ALL ", $crate::store::bearing::bearing!(@arm $phase, $n; $kind $($as),*))
    };
    (@arm $phase:tt, $n:expr; each $members:expr, $cond:expr) => {
        concat!(
            "SELECT en.event FROM (",
            $members,
            ") m JOIN event_node en ON en.node=m.node WHERE en.kind!='tested' AND ",
            $cond,
            " AND NOT ",
            $crate::store::bearing::owns!($phase, "en.event", $n)
        )
    };
    (@arm $phase:tt, $n:expr; tests $members:expr, $cond:expr) => {
        concat!(
            "SELECT t.event FROM (",
            $members,
            ") m JOIN test t ON t.candidate=m.node WHERE ",
            $cond,
            " AND NOT ",
            $crate::store::bearing::owns!($phase, "t.event", $n)
        )
    };
    (@arm $phase:tt, $n:expr; environment $cond:expr) => {
        concat!(
            "SELECT ev.event FROM environment ev WHERE ",
            $cond,
            " AND NOT ",
            $crate::store::bearing::owns!($phase, "ev.event", $n)
        )
    };
    (@latest $phase:tt, $n:expr $(; $kinds:ident $($as:expr),*)*) => {
        $crate::store::bearing::bearing!(@max last,
            concat!("SELECT max(eb.event) FROM ", $crate::store::bearing::bearing!(@stored $phase, $n)),
            $phase, $n $(; $kinds $($as),*)*)
    };
    // What a standing result is current against: the bearing but its disputes, and the offers
    // that, like them, give its work fresh tries.
    (@rested $phase:tt, $n:expr $(; $kinds:ident $($as:expr),*)*) => {
        $crate::store::bearing::bearing!(@max rest,
            concat!(
                "SELECT eb.event FROM ",
                $crate::store::bearing::bearing!(@stored $phase, $n),
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
                $crate::store::bearing::bearing!(@$last $phase, $n; $kinds $($as),*), ")",)+
            "))"
        )
    };
    (@rest $phase:tt, $n:expr; offers) => {
        "NULL"
    };
    (@rest $phase:tt, $n:expr; $kind:ident $($as:expr),*) => {
        $crate::store::bearing::bearing!(@last $phase, $n; $kind $($as),*)
    };
    (@last $phase:tt, $n:expr; each $members:expr, $cond:expr) => {
        concat!(
            "SELECT max((SELECT en.event FROM event_node en WHERE en.node=m.node AND \
             en.kind!='tested' AND ",
            $cond,
            " AND NOT ",
            $crate::store::bearing::owns!($phase, "en.event", $n),
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
            $crate::store::bearing::reported_as!("ot.began", $n)
        )
    };
    // A test's id orders it as its event does.
    (@last $phase:tt, $n:expr; tests $members:expr, $cond:expr) => {
        concat!(
            "SELECT max((SELECT t.event FROM test t WHERE t.candidate=m.node AND ",
            $cond,
            " AND NOT ",
            $crate::store::bearing::owns!($phase, "t.event", $n),
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
            $crate::store::bearing::owns!($phase, "ev.event", $n),
            " ORDER BY ev.event DESC LIMIT 1"
        )
    };
    ($phase:tt, $n:expr) => {
        $crate::store::bearing::bearing!(@arms $phase, $n, stream)
    };
}
pub(super) use bearing;

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
pub(super) use reported_as;

/// Record root `root`'s reportable members as event `event` of its report work leaves them.
pub(super) fn record_scenarios(db: &Db, event: EventId, root: impl Into<NodeId>) -> Result<()> {
    let root = root.into();
    db.execute(
        "INSERT INTO report_scenario(event,root,candidate) SELECT ?1,root,candidate FROM \
         finding_scenario WHERE root=?2",
        params![event, root],
    )?;
    Ok(())
}

/// What `$phase`'s result on node `$n`, recorded by event `$event`, stands on: the basis of the
/// task of `$phase` on it the result was recorded within. A result recorded outside one stands on
/// nothing, so its phase owes it.
macro_rules! stamp {
    ($phase:tt, $event:expr, $n:expr) => {
        concat!(
            "(SELECT wk.basis FROM event_work wk WHERE wk.event=",
            $event,
            " AND wk.phase='",
            $phase,
            "' AND wk.node=",
            $n,
            ")"
        )
    };
}
pub(super) use stamp;

/// Event `$event` was recorded within a task of `$phase` on node `$n`.
macro_rules! owns {
    ($phase:tt, $event:expr, $n:expr) => {
        concat!(
            "EXISTS (SELECT 1 FROM event_work ow WHERE ow.event=",
            $event,
            " AND ow.phase='",
            $phase,
            "' AND ow.node=",
            $n,
            ")"
        )
    };
}
pub(super) use owns;

/// The event of the latest result in `$table` on node `$n`, by its column `$column`, that
/// `$counts` holds for, a condition on its event `x.event`.
macro_rules! standing {
    ($table:literal, $column:literal, $n:expr, $counts:expr) => {
        concat!(
            "(SELECT x.event FROM ",
            $table,
            " x WHERE x.",
            $column,
            "=",
            $n,
            " AND ",
            $counts,
            " ORDER BY x.event DESC LIMIT 1)"
        )
    };
}
pub(super) use standing;

/// The event of the latest result in `$table` on node `$n` that the work owning it recorded.
macro_rules! owned {
    ($table:literal, $column:literal, $n:expr) => {
        $crate::store::bearing::standing!($table, $column, $n, "x.owned")
    };
}
pub(super) use owned;

/// The event of the latest result in `$table` on node `$n` after its standing one at `$event`
/// whose `$value` differs from the standing `$held`: a proposal recorded outside the work owning
/// it, which that work decides. One that agrees restates the standing, and one repeating an
/// earlier proposal was answered while the first result the work owning it recorded after that
/// one stands on `$phase`'s current basis: anything moving the basis since asks it again.
macro_rules! proposed {
    ($table:literal, $column:literal, $n:expr, $event:expr, $value:literal, $held:expr,
        $phase:tt) => {
        concat!(
            "(SELECT y.event FROM ",
            $table,
            " y WHERE y.",
            $column,
            "=",
            $n,
            " AND y.event>ifnull(",
            $event,
            ",0) AND y.",
            $value,
            " IS NOT ",
            $held,
            " AND NOT EXISTS (SELECT 1 FROM ",
            $table,
            " z WHERE z.",
            $column,
            "=",
            $n,
            " AND z.",
            $value,
            " IS y.",
            $value,
            " AND z.event<y.event AND NOT z.owned AND (SELECT ",
            $crate::store::bearing::stamp!($phase, "x.event", $n),
            " FROM ",
            $table,
            " x WHERE x.",
            $column,
            "=",
            $n,
            " AND x.owned AND x.event>z.event AND x.event<y.event ORDER BY x.event LIMIT 1)=",
            $crate::store::bearing::basis!($phase, $n),
            ") ORDER BY y.event DESC LIMIT 1)"
        )
    };
}
pub(super) use proposed;

/// Row `$a` of `premise_standing` or `flaw_standing`, whose standing was set at its column
/// `$stamp`, is disputed: a proposal disagrees with it, or contradicting evidence came after it.
macro_rules! disputed {
    ($a:literal, $stamp:literal) => {
        concat!(
            "(",
            $a,
            ".proposed IS NOT NULL OR EXISTS (SELECT 1 FROM evidence d WHERE d.node=",
            $a,
            ".node AND d.stance='contradicts' AND d.event>ifnull(",
            $a,
            ".",
            $stamp,
            ",0)))"
        )
    };
}
pub(super) use disputed;

/// The latest decision or dispute of a premise that premise `$p` rests on, but those recorded
/// within its own settle: the arm of its settle's bearing on what it rests on, as a statement.
macro_rules! undermined {
    ($p:expr) => {
        $crate::store::bearing::bearing!(@last "settle", $p;
            each $crate::store::bearing::bearing!(@rests $p), $crate::store::bearing::bearing!(@rests))
    };
}
pub(super) use undermined;

/// The basis of `$phase`'s work on node `$n`: the latest event of its bearing, or the node's
/// latest event while its bearing holds none, as an untested candidate's test, so that work
/// owed always rests on one. `rested` leaves out its `disputes`: what a standing result is
/// current against.
macro_rules! basis {
    ($phase:tt, $n:expr) => {
        $crate::store::bearing::basis!(latest $phase, $n)
    };
    ($mode:ident $phase:tt, $n:expr) => {
        concat!(
            "coalesce(",
            $crate::store::bearing::bearing!(@arms $phase, $n, $mode),
            ",(SELECT max(event) FROM event_node WHERE node=",
            $n,
            "))"
        )
    };
}
pub(super) use basis;

/// Path `$p`'s reads of recorded states its caller supplies, input or handed back, as `$a` rows
/// of `path_state`, to follow a `FROM`; a `JOIN` may follow.
macro_rules! driven_reads {
    ($a:literal, $p:expr) => {
        concat!(
            "path_state ",
            $a,
            " JOIN state ",
            $a,
            "s ON ",
            $a,
            "s.node=",
            $a,
            ".state AND ",
            $a,
            ".path=",
            $p,
            " AND ",
            $a,
            ".access='read' AND ",
            $a,
            "s.scope IN ('input','handed') AND ",
            $a,
            "s.status='recorded'"
        )
    };
}
pub(super) use driven_reads;

/// Candidate `pa.candidate` performs a path that relies on premise `pp.premise`, where
/// `$filter` holds: a retracted path stands in no candidate's chain.
macro_rules! action_premises {
    ($filter:expr) => {
        concat!(
            "candidate_action pa JOIN path_premise pp ON pp.path=pa.path JOIN path ap ON \
             ap.node=pa.path AND ap.status!='retracted' WHERE ",
            $filter
        )
    };
}
pub(super) use action_premises;

/// The premises candidate `$c`'s chain rests on: its flaws' and those of every path it performs.
macro_rules! chain_premises {
    ($c:expr) => {
        concat!(
            "SELECT fp.premise FROM candidate_flaw cf JOIN flaw_premise fp ON fp.flaw=cf.flaw \
             WHERE cf.candidate=",
            $c,
            " UNION SELECT pp.premise FROM ",
            $crate::store::bearing::action_premises!(concat!("pa.candidate=", $c))
        )
    };
}
pub(super) use chain_premises;

/// What a test of candidate `$c` recorded at event `$event` demonstrates is unchanged since its
/// work began, or since it was recorded outside a task: no edit of the test's bearing came after.
macro_rules! tested_unchanged {
    ($c:expr, $event:expr) => {
        concat!(
            "ifnull(",
            $crate::store::bearing::edited!("test", $c),
            ",0)<=coalesce(",
            $crate::store::bearing::stamp!("test", $event, $c),
            ",",
            $event,
            ")"
        )
    };
}
pub(super) use tested_unchanged;

/// The test candidate `$c`'s testing stands on: its latest that is not pending when that is a
/// decline, which stands across verdicts and changes, or else its latest of what is unchanged.
macro_rules! settling_test {
    ($c:expr) => {
        concat!(
            "(SELECT CASE tl.result WHEN 'declined' THEN tl.id ELSE (SELECT max(sx.id) FROM test \
             sx WHERE sx.candidate=",
            $c,
            " AND sx.result!='pending' AND ",
            $crate::store::bearing::tested_unchanged!($c, "sx.event"),
            ") END FROM (SELECT id,result FROM test WHERE candidate=",
            $c,
            " AND result!='pending' ORDER BY id DESC LIMIT 1) tl)"
        )
    };
}
pub(super) use settling_test;

/// Candidate `$c` has recorded no test since event `$began`: its latest test, if any, came
/// before.
macro_rules! untested_since {
    ($c:expr, $began:expr) => {
        concat!(
            "ifnull((SELECT ux.event FROM test ux WHERE ux.candidate=",
            $c,
            " ORDER BY ux.id DESC LIMIT 1),0)<",
            $began
        )
    };
}
pub(super) use untested_since;

/// The candidates whose chain rests on premise `$p`.
macro_rules! premise_candidates {
    ($p:expr) => {
        concat!(
            "SELECT cf.candidate FROM flaw_premise fp JOIN candidate_flaw cf ON cf.flaw=fp.flaw \
             WHERE fp.premise=",
            $p,
            " UNION SELECT pa.candidate FROM ",
            $crate::store::bearing::action_premises!(concat!("pp.premise=", $p))
        )
    };
}
pub(super) use premise_candidates;

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
pub(super) use retirement;

/// The latest material edit of node `$n`, a merge into it included, but those made within a
/// task of `$phase`, which decides it, and the merge retiring it, which leaves its standing as
/// it stood.
macro_rules! edited {
    ($phase:tt, $n:expr) => {
        concat!(
            "(SELECT eb.event FROM ",
            $crate::store::bearing::bearing!(@stored $phase, $n),
            " AND eb.role='edits' AND eb.event IS NOT ",
            $crate::store::bearing::retirement!($n),
            " ORDER BY eb.event DESC LIMIT 1)"
        )
    };
}
pub(super) use edited;

/// The standing of candidate `s`, a row of `candidate_core`, where `$noted` holds while another
/// live member of its root group stands validated and demonstrated at its severity or above.
macro_rules! state {
    ($noted:expr) => {
        concat!(
            "CASE WHEN s.merged_into IS NOT NULL THEN 'merged' WHEN NOT s.assessed_current THEN \
             'identified' WHEN NOT s.shortlisted THEN 'below_threshold' WHEN NOT \
             s.reviewed_current THEN 'shortlisted' WHEN s.verdict!='validated' THEN s.verdict \
             WHEN NOT s.demonstrated AND ",
            $noted,
            " THEN 'noted' ELSE 'validated' END"
        )
    };
}
pub(super) use state;

/// A candidate's standing as its results last left it, current or not, for display: unscored
/// only when never assessed. `$noted` is as in [`state`].
macro_rules! standing_state {
    ($noted:expr) => {
        concat!(
            "CASE WHEN s.merged_into IS NOT NULL THEN 'merged' WHEN s.severity IS NULL THEN \
             'identified' WHEN NOT s.shortlisted THEN 'below_threshold' WHEN s.verdict IS NULL \
             THEN 'shortlisted' WHEN s.verdict!='validated' THEN s.verdict WHEN NOT \
             s.demonstrated AND ",
            $noted,
            " THEN 'noted' ELSE 'validated' END"
        )
    };
}
pub(super) use standing_state;

/// Whether another live member of a candidate's root group stands validated and demonstrated
/// at its severity or above, which notes a validated candidate.
macro_rules! noted {
    () => {
        "EXISTS (SELECT 1 FROM candidate_root o JOIN candidate_core d ON d.candidate=o.candidate \
         WHERE o.root=s.root AND o.candidate!=s.candidate AND d.merged_into IS NULL AND d.upheld \
         AND d.demonstrated AND d.rank>=s.rank)"
    };
}
pub(super) use noted;

/// The facts of the root groups `$roots` selects, every group without it, read once per
/// statement: their live candidates whose positive verdict and assessment stand, and the
/// highest severity each group demonstrated.
macro_rules! demonstrations {
    () => {
        $crate::store::bearing::demonstrations!(@ "candidate_core u")
    };
    ($roots:expr) => {
        $crate::store::bearing::demonstrations!(@ concat!(
            "(", $roots, ") ur JOIN candidate_flaw uf ON uf.flaw=ur.root AND uf.position=0 JOIN \
             candidate_core u ON u.candidate=uf.candidate"
        ))
    };
    (@ $upheld:expr) => {
        concat!(
            "upheld AS MATERIALIZED (SELECT u.candidate,u.root,u.rank,u.shortlisted,\
             u.demonstrated FROM ",
            $upheld,
            " WHERE u.verdict='validated' AND u.merged_into IS NULL AND u.upheld),\
             demonstration(root,rank) AS MATERIALIZED (SELECT root,max(rank) FROM upheld WHERE \
             demonstrated GROUP BY root)"
        )
    };
}
pub(super) use demonstrations;

/// The views deriving each node's standing from its results and what they rest on.
macro_rules! derived_views {
    () => {
        concat!(
            "
-- A premise's or flaw's standing is its latest status set by the work owning it: a code
-- premise's settle, a deployment premise's answer outside every task, a flaw's chain. It reads
-- undecided or suspected while none was, or after a material edit since. A later status set
-- elsewhere that differs from it is a proposal, which disputes it, unless its work already
-- answered that status proposed before.
CREATE VIEW premise_standing AS
SELECT s.*,",
            $crate::store::bearing::proposed!(
                "premise_decision",
                "premise",
                "s.node",
                "s.decided",
                "status",
                "s.status",
                "settle"
            ),
            " AS proposed FROM (SELECT p.node,p.scope,p.location,d.event AS decided,CASE WHEN \
             d.event>=ifnull(",
            $crate::store::bearing::edited!("settle", "p.node"),
            ",0) THEN d.status ELSE 'undecided' END AS status FROM premise p LEFT JOIN \
             premise_decision d ON d.event=",
            $crate::store::bearing::owned!("premise_decision", "premise", "p.node"),
            ") s;
CREATE VIEW flaw_standing AS
SELECT s.*,",
            $crate::store::bearing::proposed!(
                "flaw_judgement",
                "flaw",
                "s.node",
                "s.judged",
                "status",
                "s.status",
                "chain"
            ),
            " AS proposed FROM (SELECT f.node,f.component,f.location,f.cwe,d.event AS judged,CASE \
             WHEN d.event>=ifnull(",
            $crate::store::bearing::edited!("chain", "f.node"),
            ",0) THEN d.status ELSE 'suspected' END AS status FROM flaw f LEFT JOIN flaw_judgement \
             d ON d.event=",
            $crate::store::bearing::owned!("flaw_judgement", "flaw", "f.node"),
            ") s;

-- Each candidate's standing assessment and verdict, the latest recorded within assess and
-- review work on it, and the latest of each recorded since elsewhere that differs and that work
-- has not answered: a proposal its phase decides.
CREATE VIEW candidate_result AS
SELECT c.node AS candidate,a.event AS assessed,a.vector,a.severity,",
            $crate::store::bearing::proposed!(
                "candidate_assessment",
                "candidate",
                "c.node",
                "a.event",
                "vector",
                "a.vector",
                "assess"
            ),
            " AS assessment_proposed,v.event AS reviewed,v.verdict,",
            $crate::store::bearing::proposed!(
                "candidate_verdict",
                "candidate",
                "c.node",
                "v.event",
                "verdict",
                "v.verdict",
                "review"
            ),
            " AS verdict_proposed FROM candidate c LEFT JOIN candidate_assessment a ON a.event=",
            $crate::store::bearing::owned!("candidate_assessment", "candidate", "c.node"),
            " LEFT JOIN candidate_verdict v ON v.event=",
            $crate::store::bearing::owned!("candidate_verdict", "candidate", "c.node"),
            ";

-- Each candidate's results judged against what they rest on. A result is current while no
-- event of its phase's bearing but a proposal disputing it came after what it stands on; a
-- verdict also only while the standings it claims hold: validated, no premise of its chain
-- undecided or failing and no chain flaw refuted; disproved, a code premise failing or a chain
-- flaw refuted; excluded, a deployment premise failing; unproven, a premise undecided.
CREATE VIEW candidate_core AS
SELECT b.*,b.assessed_current AND b.reviewed_current AND ifnull(b.verdict='validated',0) AS upheld
FROM (SELECT g.*,g.reviewed_at_basis AND CASE g.verdict WHEN 'validated' THEN NOT (g.undecided OR
    g.contradicted) WHEN 'disproved' THEN g.refuted OR g.code_fails WHEN 'excluded' THEN
    g.deployment_fails ELSE g.undecided END AS reviewed_current,
    g.confirmed AND NOT g.contradicted AS demonstrated
FROM (SELECT f.*,f.refuted OR f.code_fails OR f.deployment_fails AS contradicted FROM (
SELECT c.candidate,n.merged_into,r.root,c.severity,k.rank,
    k.rank>=(SELECT t.rank FROM run JOIN severity_kind t ON t.name=run.threshold) AS shortlisted,
    c.vector,c.assessed,c.assessment_proposed,
    ifnull(",
            $crate::store::bearing::stamp!("assess", "c.assessed", "c.candidate"),
            ">=",
            $crate::store::bearing::basis!(rested "assess", "c.candidate"),
            ",0) AS assessed_current,
    c.verdict,c.reviewed,c.verdict_proposed,
    ifnull(",
            $crate::store::bearing::stamp!("review", "c.reviewed", "c.candidate"),
            ">=",
            $crate::store::bearing::basis!(rested "review", "c.candidate"),
            ",0) AS reviewed_at_basis,
    EXISTS (SELECT 1 FROM candidate_flaw rf JOIN flaw_standing fs ON fs.node=rf.flaw
        WHERE rf.candidate=c.candidate AND fs.status='refuted') AS refuted,
    EXISTS (SELECT 1 FROM (",
            $crate::store::bearing::chain_premises!("c.candidate"),
            ") cp JOIN premise_standing p ON p.node=cp.premise WHERE p.status='fails' AND
        p.scope='code') AS code_fails,
    EXISTS (SELECT 1 FROM (",
            $crate::store::bearing::chain_premises!("c.candidate"),
            ") cp JOIN premise_standing p ON p.node=cp.premise WHERE p.status='fails' AND
        p.scope='deployment') AS deployment_fails,
    EXISTS (SELECT 1 FROM (",
            $crate::store::bearing::chain_premises!("c.candidate"),
            ") cp JOIN premise_standing p ON p.node=cp.premise WHERE p.status='undecided') AS undecided,
    ifnull((SELECT result='confirmed' FROM test WHERE id=",
            $crate::store::bearing::settling_test!("c.candidate"),
            "),0) AS confirmed
FROM candidate_result c JOIN node n ON n.id=c.candidate LEFT JOIN candidate_root r ON
    r.candidate=c.candidate LEFT JOIN severity_kind k ON k.name=c.severity) f) g) b;

-- Each candidate's standing, derived. A validated candidate is noted while another live
-- member of its root group stands validated and demonstrated at its severity or above. For
-- display, its standing as its results last left it, and whether one of them no longer stands.
CREATE VIEW candidate_state AS
SELECT s.*,",
            $crate::store::bearing::state!($crate::store::bearing::noted!()),
            " AS state,",
            $crate::store::bearing::standing_state!($crate::store::bearing::noted!()),
            " AS standing,
    s.merged_into IS NULL AND s.severity IS NOT NULL AND (NOT s.assessed_current OR
        (s.verdict IS NOT NULL AND NOT s.reviewed_current)) AS stale,
    CASE WHEN s.merged_into IS NOT NULL OR NOT s.assessed_current THEN NULL
        WHEN s.upheld THEN
            CASE WHEN s.demonstrated THEN 'demonstrated' ELSE 'confirmed' END
        WHEN s.undecided OR s.contradicted THEN 'conjectured'
        ELSE 'established' END AS confidence
FROM candidate_core s;

-- The scenarios a root's finding reports.
CREATE VIEW finding_scenario AS
SELECT r.root,s.candidate,s.state,s.confidence,s.severity,s.rank
FROM candidate_root r JOIN candidate_state s ON s.candidate=r.candidate
WHERE s.state IN ('validated','noted');
"
        )
    };
}
pub(super) use derived_views;

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
pub(super) const fn lookup(phase: super::Phase) -> &'static str {
    use super::Phase;
    match phase {
        Phase::Sweep => lookup!("sweep"),
        Phase::Probe => lookup!("probe"),
        Phase::Check => lookup!("check"),
        Phase::Settle => lookup!("settle"),
        Phase::Chain => lookup!("chain"),
        Phase::Assess => lookup!("assess"),
        Phase::Review => lookup!("review"),
        Phase::Test => lookup!("test"),
        Phase::Report => lookup!("report"),
    }
}

//! Relations read per node. What each phase's work on a node reads: the events that move it,
//! whose latest is the basis and whose later ones are notices; and the premises a candidate's
//! chain rests on. Each is spelled with the node in every arm, because SQLite pushes no
//! correlated term into a compound view: read for each subject of a listing, a view would scan
//! its tables whole for each. The derived views reading them are generated here too.

/// The events of `$phase`'s bearing on node `$n`, but those recorded within a task of
/// `$phase` on it, by its agent or one beneath it: a phase's own work never moves what it rests
/// on, a pending test's record aside. Each phase's bearing is a list of arms: `on` events
/// bearing on `$n` itself, `each` on every node a query selects as `node`, `tests` the test
/// records of `$n` or of each such node, and `environment` environments recorded. Test records
/// bear on their candidate through `tests` alone, never `on` or `each`, which read a node's
/// effects down an index holding none of them. Notices read the arms whole as this stream; the
/// [`basis`] reads each newest first down its index and stops at its first event that counts,
/// past none older.
macro_rules! bearing {
    (@arms "sweep", $n:expr, $then:ident) => {
        $crate::store::bearing::bearing!(@$then "sweep", $n;
            on "en.effect='standing'"; on "en.effect='changed'")
    };
    // A probe also rests on the environments for any candidate serving an interface its recorded
    // sources sit on, or any such one when they sit on none, but not one recorded within a probe: a
    // new one gives an unfinished probe fresh tries.
    (@arms "probe", $n:expr, $then:ident) => {
        $crate::store::bearing::bearing!(@$then "probe", $n;
            on "en.effect='standing'"; on "en.effect='changed'";
            environment concat!(
                "ev.candidate IS NULL AND NOT EXISTS (SELECT 1 FROM event_task pt CROSS JOIN \
                 task_node pn ON pn.task=pt.task WHERE pt.event=ev.event AND +pn.phase='probe') \
                 AND coalesce((SELECT max(EXISTS (SELECT 1 FROM environment_interface ei WHERE \
                 ei.environment=ev.id AND ei.interface=pi.interface)) FROM path_source ps JOIN \
                 source po ON po.node=ps.source AND po.status='recorded' JOIN interface_location \
                 pi ON pi.location=po.location WHERE ps.path=",
                $n,
                "),1)"
            ))
    };
    (@arms "settle", $n:expr, $then:ident) => {
        $crate::store::bearing::bearing!(@$then "settle", $n;
            on "en.effect='standing'"; on "en.effect='changed'")
    };
    // A chain also rests on the verdicts of the candidates holding its flaw, and on the chain
    // edits it joins or leaves: a disproval or a leave releases it with fresh tries.
    (@arms "chain", $n:expr, $then:ident) => {
        $crate::store::bearing::bearing!(@$then "chain", $n;
            on "en.effect='standing'"; on "en.effect='changed'";
            each concat!("SELECT candidate AS node FROM candidate_flaw WHERE flaw=", $n),
                "en.effect='standing' AND en.kind='verdict'";
            on "en.effect='concerns' AND en.kind='edited'")
    };
    (@arms "assess", $n:expr, $then:ident) => {
        $crate::store::bearing::bearing!(@$then "assess", $n; on "en.effect='changed'")
    };
    // A test's record leaves the verdict standing.
    (@arms "review", $n:expr, $then:ident) => {
        $crate::store::bearing::bearing!(@$then "review", $n;
            on "en.effect='standing'"; on "en.effect='changed'")
    };
    // A test rests on the verdict, the assessment, the latest attempt and the environments
    // for its candidate or for any.
    (@arms "test", $n:expr, $then:ident) => {
        $crate::store::bearing::bearing!(@$then "test", $n;
            on "en.effect='standing' AND en.kind IN ('verdict','assessed')";
            tests;
            environment concat!("ev.candidate=", $n);
            environment "ev.candidate IS NULL")
    };
    // A report rests on its root's own standing changes, which a candidate taking or losing it
    // as its root makes, a merge included, and on every current member's, a settled test's
    // included.
    (@arms "report", $n:expr, $then:ident) => {
        $crate::store::bearing::bearing!(@$then "report", $n;
            on "en.effect='standing'"; on "en.effect='changed'";
            each $crate::store::bearing::bearing!(@members $n), "en.effect='standing'";
            each $crate::store::bearing::bearing!(@members $n), "en.effect='changed'";
            tests $crate::store::bearing::bearing!(@members $n), "t.result!='pending'")
    };
    (@members $n:expr) => {
        concat!(
            "SELECT r.candidate AS node FROM candidate_root r JOIN node rn ON rn.id=r.candidate \
             WHERE rn.merged_into IS NULL AND r.root=",
            $n
        )
    };
    (@stream $phase:tt, $n:expr; $kind:ident $($a:expr),* $(; $kinds:ident $($as:expr),*)*) => {
        concat!(
            "SELECT ob.event FROM (",
            $crate::store::bearing::bearing!(@arm $n; $kind $($a),*),
            $(" UNION ALL ", $crate::store::bearing::bearing!(@arm $n; $kinds $($as),*),)*
            ") ob WHERE ",
            $crate::store::bearing::bearing!(@keep $phase, "ob.event", $n)
        )
    };
    (@arm $n:expr; on $cond:expr) => {
        concat!(
            "SELECT en.event FROM event_node en WHERE en.node=",
            $n,
            " AND en.kind!='tested' AND ",
            $cond
        )
    };
    (@arm $n:expr; each $members:expr, $cond:expr) => {
        concat!(
            "SELECT en.event FROM (",
            $members,
            ") m JOIN event_node en ON en.node=m.node WHERE en.kind!='tested' AND ",
            $cond
        )
    };
    (@arm $n:expr; tests) => {
        concat!("SELECT t.event FROM test t WHERE t.candidate=", $n)
    };
    (@arm $n:expr; tests $members:expr, $cond:expr) => {
        concat!(
            "SELECT t.event FROM (",
            $members,
            ") m JOIN test t ON t.candidate=m.node WHERE ",
            $cond
        )
    };
    (@arm $n:expr; environment $cond:expr) => {
        concat!("SELECT ev.event FROM environment ev WHERE ", $cond)
    };
    (@latest $phase:tt, $n:expr; $kind:ident $($a:expr),* $(; $kinds:ident $($as:expr),*)*) => {
        concat!(
            "(SELECT max(event) FROM (SELECT (",
            $crate::store::bearing::bearing!(@last $phase, $n; $kind $($a),*),
            ") AS event",
            $(" UNION ALL SELECT (",
                $crate::store::bearing::bearing!(@last $phase, $n; $kinds $($as),*), ")",)*
            "))"
        )
    };
    (@last $phase:tt, $n:expr; on $cond:expr) => {
        concat!(
            "SELECT en.event FROM event_node en WHERE en.node=",
            $n,
            " AND en.kind!='tested' AND ",
            $cond,
            " AND (",
            $crate::store::bearing::bearing!(@keep $phase, "en.event", $n),
            ") ORDER BY en.event DESC LIMIT 1"
        )
    };
    (@last $phase:tt, $n:expr; each $members:expr, $cond:expr) => {
        concat!(
            "SELECT max((SELECT en.event FROM event_node en WHERE en.node=m.node AND \
             en.kind!='tested' AND ",
            $cond,
            " AND (",
            $crate::store::bearing::bearing!(@keep $phase, "en.event", $n),
            ") ORDER BY en.event DESC LIMIT 1)) FROM (",
            $members,
            ") m"
        )
    };
    // A test's id orders it as its event does.
    (@last $phase:tt, $n:expr; tests) => {
        concat!(
            "SELECT t.event FROM test t WHERE t.candidate=",
            $n,
            " AND (",
            $crate::store::bearing::bearing!(@keep $phase, "t.event", $n),
            ") ORDER BY t.id DESC LIMIT 1"
        )
    };
    (@last $phase:tt, $n:expr; tests $members:expr, $cond:expr) => {
        concat!(
            "SELECT max((SELECT t.event FROM test t WHERE t.candidate=m.node AND ",
            $cond,
            " AND (",
            $crate::store::bearing::bearing!(@keep $phase, "t.event", $n),
            ") ORDER BY t.id DESC LIMIT 1)) FROM (",
            $members,
            ") m"
        )
    };
    (@last $phase:tt, $n:expr; environment $cond:expr) => {
        concat!(
            "SELECT ev.event FROM environment ev WHERE ",
            $cond,
            " AND (",
            $crate::store::bearing::bearing!(@keep $phase, "ev.event", $n),
            ") ORDER BY ev.event DESC LIMIT 1"
        )
    };
    // A pending test is resumed rather than repeated, so its record moves the test's basis
    // even within its own work.
    (@keep "test", $event:expr, $n:expr) => {
        concat!(
            "NOT EXISTS (SELECT 1 FROM ",
            $crate::store::bearing::within!("test", $event, $n),
            ") OR EXISTS (SELECT 1 FROM test WHERE event=",
            $event,
            " AND candidate=",
            $n,
            " AND result='pending')"
        )
    };
    (@keep $phase:tt, $event:expr, $n:expr) => {
        concat!(
            "NOT EXISTS (SELECT 1 FROM ",
            $crate::store::bearing::within!($phase, $event, $n),
            ")"
        )
    };
    ($phase:tt, $n:expr) => {
        $crate::store::bearing::bearing!(@arms $phase, $n, stream)
    };
}
pub(super) use bearing;

/// The tasks of `$phase` on node `$n` that event `$event` was recorded within, as `wn` rows of
/// `task_node`, to follow a `FROM`. A merge is within none: it retires one node into another,
/// which work on the survivor rests on whoever made it. The event's few tasks drive the join and
/// are each sought by task, whatever statistics the planner lacks: the phase alone would seek
/// every task of it.
macro_rules! within {
    ($phase:tt, $event:expr, $n:expr) => {
        concat!(
            "event_task wt CROSS JOIN task_node wn ON wn.task=wt.task JOIN node wx ON \
             wx.id=wn.node WHERE wt.event=",
            $event,
            " AND (SELECT kind FROM event WHERE id=",
            $event,
            ")!='merged' AND +wn.phase='",
            $phase,
            "' AND coalesce(wx.merged_into,wx.id)=",
            $n
        )
    };
}
pub(super) use within;

/// What `$phase`'s result on node `$n`, recorded by event `$event`, stands on: the basis of the
/// task of `$phase` on it the result was recorded within, or the event itself.
macro_rules! stamp {
    ($phase:tt, $event:expr, $n:expr) => {
        concat!(
            "coalesce((SELECT max(wn.basis) FROM ",
            $crate::store::bearing::within!($phase, $event, $n),
            "),",
            $event,
            ")"
        )
    };
}
pub(super) use stamp;

/// The basis of `$phase`'s work on node `$n`: the latest event of its bearing, or the node's
/// latest event while its bearing holds none, as an untested candidate's test, so that work
/// owed always rests on one.
macro_rules! basis {
    ($phase:tt, $n:expr) => {
        concat!(
            "coalesce(",
            $crate::store::bearing::bearing!(@arms $phase, $n, latest),
            ",(SELECT max(event) FROM event_node WHERE node=",
            $n,
            "))"
        )
    };
}
pub(super) use basis;

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

/// The test candidate `$c`'s testing stands on: its latest that is not pending when that is a
/// decline, which stands across verdicts, or else its latest recorded at its latest verdict.
macro_rules! settling_test {
    ($c:expr) => {
        concat!(
            "(SELECT CASE tl.result WHEN 'declined' THEN tl.id ELSE (SELECT max(id) FROM test \
             WHERE candidate=",
            $c,
            " AND result!='pending' AND verdict=(SELECT reviewed FROM candidate WHERE node=",
            $c,
            ")) END FROM (SELECT id,result FROM test WHERE candidate=",
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

/// The latest material edit of node `$n`, a merge into it included, but those made within a
/// task of `$phase`, which decides it.
macro_rules! edited {
    ($phase:tt, $n:expr) => {
        concat!(
            "(SELECT ed.event FROM event_node ed WHERE ed.node=",
            $n,
            " AND ed.kind!='tested' AND ed.effect='changed' AND ed.kind IN ('edited','merged') AND \
             NOT EXISTS (SELECT 1 FROM ",
            $crate::store::bearing::within!($phase, "ed.event", $n),
            ") ORDER BY ed.event DESC LIMIT 1)"
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

/// The views deriving each candidate's standing from its results and what they rest on.
macro_rules! derived_views {
    () => {
        concat!(
            "
-- A premise's or flaw's status counts once set at or after its latest material edit; an edit
-- since reads it as undecided or suspected again.
CREATE VIEW premise_state AS
SELECT p.node,p.scope,p.location,p.decided,CASE WHEN ifnull(p.decided,0)>=ifnull(",
            $crate::store::bearing::edited!("settle", "p.node"),
            ",0) THEN p.status ELSE 'undecided' END AS status FROM premise p;
CREATE VIEW flaw_state AS
SELECT f.node,f.component,f.location,f.cwe,f.judged,CASE WHEN ifnull(f.judged,0)>=ifnull(",
            $crate::store::bearing::edited!("chain", "f.node"),
            ",0) THEN f.status ELSE 'suspected' END AS status FROM flaw f;

-- Each candidate's results judged against what they rest on. A result is current while no
-- event of its phase's bearing came after what it stands on, and an unproven verdict while a
-- premise of its chain is undecided.
CREATE VIEW candidate_core AS
SELECT b.*,b.reviewed_at_basis AND (b.verdict!='unproven' OR b.undecided) AS reviewed_current,
    b.confirmed AND NOT b.contradicted AS demonstrated,
    b.assessed_current AND b.reviewed_at_basis AND ifnull(b.verdict='validated',0) AS upheld FROM (
SELECT c.node AS candidate,n.merged_into,r.root,c.severity,k.rank,
    k.rank>=(SELECT t.rank FROM run JOIN severity_kind t ON t.name=run.threshold) AS shortlisted,
    ifnull(",
            $crate::store::bearing::stamp!("assess", "c.assessed", "c.node"),
            ">=",
            $crate::store::bearing::basis!("assess", "c.node"),
            ",0) AS assessed_current,
    ifnull(",
            $crate::store::bearing::stamp!("review", "c.reviewed", "c.node"),
            ">=",
            $crate::store::bearing::basis!("review", "c.node"),
            ",0) AS reviewed_at_basis,
    c.verdict,
    EXISTS (SELECT 1 FROM candidate_flaw rf JOIN flaw_state f ON f.node=rf.flaw
        WHERE rf.candidate=c.node AND f.status='refuted')
    OR EXISTS (SELECT 1 FROM (",
            $crate::store::bearing::chain_premises!("c.node"),
            ") cp JOIN premise_state p ON p.node=cp.premise WHERE p.status='fails') AS contradicted,
    EXISTS (SELECT 1 FROM (",
            $crate::store::bearing::chain_premises!("c.node"),
            ") cp JOIN premise_state p ON p.node=cp.premise WHERE p.status='undecided') AS undecided,
    ifnull((SELECT result='confirmed' FROM test WHERE id=",
            $crate::store::bearing::settling_test!("c.node"),
            "),0) AS confirmed
FROM candidate c JOIN node n ON n.id=c.node LEFT JOIN candidate_root r ON r.candidate=c.node
LEFT JOIN severity_kind k ON k.name=c.severity) b;

-- Each candidate's standing, derived. A validated candidate is noted while another live
-- member of its root group stands validated and demonstrated at its severity or above.
CREATE VIEW candidate_state AS
SELECT s.*,",
            $crate::store::bearing::state!(
                "EXISTS (SELECT 1 FROM candidate_root o JOIN candidate_core d ON \
                 d.candidate=o.candidate WHERE o.root=s.root AND o.candidate!=s.candidate AND \
                 d.merged_into IS NULL AND d.upheld AND d.demonstrated AND d.rank>=s.rank)"
            ),
            " AS state,
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
        Phase::Settle => lookup!("settle"),
        Phase::Chain => lookup!("chain"),
        Phase::Assess => lookup!("assess"),
        Phase::Review => lookup!("review"),
        Phase::Test => lookup!("test"),
        Phase::Report => lookup!("report"),
    }
}

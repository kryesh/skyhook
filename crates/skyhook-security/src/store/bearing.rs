//! Relations read per node. What each phase's work on a node reads: the events that move it,
//! whose latest is the basis and whose later ones are notices; and the premises a candidate's
//! chain rests on. Each is spelled with the node in every arm, because SQLite pushes no
//! correlated term into a compound view: read for each subject of a listing, a view would scan
//! its tables whole for each. The derived views reading them are generated here too.

/// The events of `$phase`'s bearing on node `$n`, but those recorded within a task of
/// `$phase` on it, by its agent or one beneath it: a phase's own work never moves what it rests
/// on, a pending test's record aside.
macro_rules! bearing {
    (@arms "sweep", $n:expr) => {
        $crate::store::bearing::bearing!(@moved $n)
    };
    (@arms "settle", $n:expr) => {
        $crate::store::bearing::bearing!(@moved $n)
    };
    // A chain also rests on the verdicts of the candidates holding its flaw, and on the chain
    // edits it joins or leaves: a disproval or a leave releases it with fresh tries.
    (@arms "chain", $n:expr) => {
        concat!(
            $crate::store::bearing::bearing!(@moved $n),
            " UNION ALL SELECT en.event FROM candidate_flaw cf JOIN event_node en ON \
             en.node=cf.candidate JOIN event e ON e.id=en.event WHERE cf.flaw=",
            $n,
            " AND e.kind='verdict' UNION ALL SELECT en.event FROM event_node en JOIN event e ON \
             e.id=en.event WHERE en.node=",
            $n,
            " AND en.effect='concerns' AND e.kind='edited'"
        )
    };
    (@arms "assess", $n:expr) => {
        concat!("SELECT event FROM event_node WHERE node=", $n, " AND effect='changed'")
    };
    // A test's record leaves the verdict standing.
    (@arms "review", $n:expr) => {
        concat!(
            "SELECT en.event FROM event_node en JOIN event e ON e.id=en.event WHERE en.node=",
            $n,
            " AND en.effect IN ('standing','changed') AND e.kind!='tested'"
        )
    };
    // A test rests on the verdict, the assessment, the latest attempt and the environments
    // for its candidate or for any.
    (@arms "test", $n:expr) => {
        concat!(
            "SELECT en.event FROM event_node en JOIN event e ON e.id=en.event WHERE en.node=",
            $n,
            " AND e.kind IN ('verdict','assessed','tested') UNION ALL SELECT event FROM \
             environment WHERE candidate=",
            $n,
            " UNION ALL SELECT event FROM environment WHERE candidate IS NULL"
        )
    };
    // A report rests on its root's own standing changes, which a candidate taking or losing it
    // as its root makes, a merge included, and on every current member's.
    (@arms "report", $n:expr) => {
        concat!(
            $crate::store::bearing::bearing!(@moved $n),
            " UNION ALL SELECT en.event FROM candidate_root r JOIN node rn ON rn.id=r.candidate \
             JOIN event_node en ON en.node=r.candidate WHERE rn.merged_into IS NULL AND r.root=",
            $n,
            " AND en.effect IN ('standing','changed')"
        )
    };
    (@moved $n:expr) => {
        concat!(
            "SELECT event FROM event_node WHERE node=",
            $n,
            " AND effect IN ('standing','changed')"
        )
    };
    // A pending test is resumed rather than repeated, so its record moves the test's basis
    // even within its own work.
    (@own "test", $n:expr) => {
        concat!(
            " OR ob.event IN (SELECT event FROM test WHERE candidate=",
            $n,
            " AND result='pending')"
        )
    };
    (@own $phase:tt, $n:expr) => {
        ""
    };
    ($phase:tt, $n:expr) => {
        concat!(
            "SELECT ob.event FROM (",
            $crate::store::bearing::bearing!(@arms $phase, $n),
            ") ob WHERE NOT EXISTS (SELECT 1 FROM ",
            $crate::store::bearing::within!($phase, "ob.event", $n),
            ")",
            $crate::store::bearing::bearing!(@own $phase, $n)
        )
    };
}
pub(super) use bearing;

/// The tasks of `$phase` on node `$n` that event `$event` was recorded within, as `wn` rows of
/// `task_node`, to follow a `FROM`. A merge is within none: it retires one node into another,
/// which work on the survivor rests on whoever made it.
macro_rules! within {
    ($phase:tt, $event:expr, $n:expr) => {
        concat!(
            "event_task wt JOIN task_node wn ON wn.task=wt.task JOIN node wx ON wx.id=wn.node \
             WHERE wt.event=",
            $event,
            " AND (SELECT kind FROM event WHERE id=",
            $event,
            ")!='merged' AND wn.phase='",
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
            "(SELECT coalesce(max(event),(SELECT max(event) FROM event_node WHERE node=",
            $n,
            ")) FROM (",
            $crate::store::bearing::bearing!($phase, $n),
            "))"
        )
    };
}
pub(super) use basis;

/// The premises candidate `$c`'s chain rests on: its flaws' and its path's.
macro_rules! chain_premises {
    ($c:expr) => {
        concat!(
            "SELECT fp.premise FROM candidate_flaw cf JOIN flaw_premise fp ON fp.flaw=cf.flaw \
             WHERE cf.candidate=",
            $c,
            " UNION SELECT pp.premise FROM candidate pc JOIN path_premise pp ON pp.path=pc.path \
             WHERE pc.node=",
            $c
        )
    };
}
pub(super) use chain_premises;

/// The test candidate `$c`'s testing stands on: its latest that is not pending when that is a
/// decline, which stands across verdicts, or else its latest recorded at its latest verdict.
macro_rules! settling_test {
    ($c:expr) => {
        concat!(
            "(SELECT max(t.id) FROM test t JOIN candidate tc ON tc.node=t.candidate WHERE \
             t.candidate=",
            $c,
            " AND t.result!='pending' AND (t.verdict=tc.reviewed OR (t.result='declined' AND \
             t.id=(SELECT max(tl.id) FROM test tl WHERE tl.candidate=t.candidate AND \
             tl.result!='pending'))))"
        )
    };
}
pub(super) use settling_test;

/// The candidates whose chain rests on premise `$p`.
macro_rules! premise_candidates {
    ($p:expr) => {
        concat!(
            "SELECT cf.candidate FROM flaw_premise fp JOIN candidate_flaw cf ON cf.flaw=fp.flaw \
             WHERE fp.premise=",
            $p,
            " UNION SELECT pc.node FROM path_premise pp JOIN candidate pc ON pc.path=pp.path \
             WHERE pp.premise=",
            $p
        )
    };
}
pub(super) use premise_candidates;

/// The latest material edit of node `$n`, a merge into it included, but those made within a
/// task of `$phase`, which decides it.
macro_rules! edited {
    ($phase:tt, $n:expr) => {
        concat!(
            "(SELECT max(ed.event) FROM event_node ed JOIN event ee ON ee.id=ed.event WHERE \
             ed.node=",
            $n,
            " AND ed.effect='changed' AND ee.kind IN ('edited','merged') AND NOT EXISTS (SELECT 1 \
             FROM ",
            $crate::store::bearing::within!($phase, "ed.event", $n),
            "))"
        )
    };
}
pub(super) use edited;

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
-- event of its phase's bearing came after what it stands on.
CREATE VIEW candidate_core AS
SELECT b.*,b.confirmed AND NOT b.contradicted AS demonstrated,
    b.assessed_current AND b.reviewed_current AND ifnull(b.verdict='validated',0) AS upheld FROM (
SELECT c.node AS candidate,n.merged_into,r.root,c.path,c.severity,k.rank,
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
            ",0) AS reviewed_current,
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
SELECT s.*,
    CASE WHEN s.merged_into IS NOT NULL THEN 'merged'
        WHEN NOT s.assessed_current THEN 'identified'
        WHEN NOT s.shortlisted THEN 'below_threshold'
        WHEN NOT s.reviewed_current THEN 'shortlisted'
        WHEN s.verdict!='validated' THEN s.verdict
        WHEN NOT s.demonstrated AND EXISTS (SELECT 1 FROM candidate_root o JOIN candidate_core d
            ON d.candidate=o.candidate WHERE o.root=s.root AND o.candidate!=s.candidate
            AND d.merged_into IS NULL AND d.upheld AND d.demonstrated AND d.rank>=s.rank)
            THEN 'noted'
        ELSE 'validated' END AS state,
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
        Phase::Settle => lookup!("settle"),
        Phase::Chain => lookup!("chain"),
        Phase::Assess => lookup!("assess"),
        Phase::Review => lookup!("review"),
        Phase::Test => lookup!("test"),
        Phase::Report => lookup!("report"),
    }
}

//! Candidate and test facts the queues and views share: standings, demonstrations, settling
//! tests and the premises a chain rests on.

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
pub(in crate::store) use driven_reads;

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
pub(in crate::store) use action_premises;

/// The premises candidate `$c`'s chain rests on: its flaws' and those of every path it performs.
macro_rules! chain_premises {
    ($c:expr) => {
        concat!(
            "SELECT fp.premise FROM candidate_flaw cf JOIN flaw_premise fp ON fp.flaw=cf.flaw \
             WHERE cf.candidate=",
            $c,
            " UNION SELECT pp.premise FROM ",
            $crate::store::ledger::facts::action_premises!(concat!("pa.candidate=", $c))
        )
    };
}
pub(in crate::store) use chain_premises;

/// What a test of candidate `$c` recorded at event `$event` demonstrates is unchanged since its
/// work began, or since it was recorded outside a task: no edit of the test's bearing came after.
macro_rules! tested_unchanged {
    ($c:expr, $event:expr) => {
        concat!(
            "ifnull(",
            $crate::store::ledger::standing::edited!("test", $c),
            ",0)<=coalesce(",
            $crate::store::ledger::standing::stamp!("test", $event, $c),
            ",",
            $event,
            ")"
        )
    };
}
pub(in crate::store) use tested_unchanged;

/// The test candidate `$c`'s testing stands on: its latest that is not pending when that is a
/// decline, which stands across verdicts and changes, or else its latest of what is unchanged.
macro_rules! settling_test {
    ($c:expr) => {
        concat!(
            "(SELECT CASE tl.result WHEN 'declined' THEN tl.id ELSE (SELECT max(sx.id) FROM test \
             sx WHERE sx.candidate=",
            $c,
            " AND sx.result!='pending' AND ",
            $crate::store::ledger::facts::tested_unchanged!($c, "sx.event"),
            ") END FROM (SELECT id,result FROM test WHERE candidate=",
            $c,
            " AND result!='pending' ORDER BY id DESC LIMIT 1) tl)"
        )
    };
}
pub(in crate::store) use settling_test;

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
pub(in crate::store) use untested_since;

/// The candidates whose chain rests on premise `$p`.
macro_rules! premise_candidates {
    ($p:expr) => {
        concat!(
            "SELECT cf.candidate FROM flaw_premise fp JOIN candidate_flaw cf ON cf.flaw=fp.flaw \
             WHERE fp.premise=",
            $p,
            " UNION SELECT pa.candidate FROM ",
            $crate::store::ledger::facts::action_premises!(concat!("pp.premise=", $p))
        )
    };
}
pub(in crate::store) use premise_candidates;

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
pub(in crate::store) use state;

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
pub(in crate::store) use standing_state;

/// Whether another live member of a candidate's root group stands validated and demonstrated
/// at its severity or above, which notes a validated candidate.
macro_rules! noted {
    () => {
        "EXISTS (SELECT 1 FROM candidate_root o JOIN candidate_core d ON d.candidate=o.candidate \
         WHERE o.root=s.root AND o.candidate!=s.candidate AND d.merged_into IS NULL AND d.upheld \
         AND d.demonstrated AND d.rank>=s.rank)"
    };
}
pub(in crate::store) use noted;

/// The facts of the root groups `$roots` selects, every group without it, read once per
/// statement: their live candidates whose positive verdict and assessment stand, and the
/// highest severity each group demonstrated.
macro_rules! demonstrations {
    () => {
        $crate::store::ledger::facts::demonstrations!(@ "candidate_core u")
    };
    ($roots:expr) => {
        $crate::store::ledger::facts::demonstrations!(@ concat!(
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
pub(in crate::store) use demonstrations;

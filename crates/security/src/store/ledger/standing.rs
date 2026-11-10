//! A result's standing: the work owning it, what it stands on, and the proposals disputing it.

use crate::store::*;

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
pub(in crate::store) use stamp;

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
pub(in crate::store) use owns;

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
pub(in crate::store) use standing;

/// The event of the latest result in `$table` on node `$n` that the work owning it recorded.
macro_rules! owned {
    ($table:literal, $column:literal, $n:expr) => {
        $crate::store::ledger::standing::standing!($table, $column, $n, "x.owned")
    };
}
pub(in crate::store) use owned;

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
            $crate::store::ledger::standing::stamp!($phase, "x.event", $n),
            " FROM ",
            $table,
            " x WHERE x.",
            $column,
            "=",
            $n,
            " AND x.owned AND x.event>z.event AND x.event<y.event ORDER BY x.event LIMIT 1)=",
            $crate::store::ledger::bearing::basis!($phase, $n),
            ") ORDER BY y.event DESC LIMIT 1)"
        )
    };
}
pub(in crate::store) use proposed;

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
pub(in crate::store) use disputed;

/// The latest decision or dispute of a premise that premise `$p` rests on, but those recorded
/// within its own settle: the arm of its settle's bearing on what it rests on, as a statement.
macro_rules! undermined {
    ($p:expr) => {
        $crate::store::ledger::bearing::bearing!(@last "settle", $p;
            each $crate::store::ledger::bearing::bearing!(@rests $p), $crate::store::ledger::bearing::bearing!(@rests))
    };
}
pub(in crate::store) use undermined;

/// The latest material edit of node `$n`, a merge into it included, but those made within a
/// task of `$phase`, which decides it, and the merge retiring it, which leaves its standing as
/// it stood. `own` keeps a candidate's own edits and merges alone: those changing no flaw or
/// path, the only nodes that carry a change to a candidate.
macro_rules! edited {
    ($phase:tt, $n:expr) => {
        $crate::store::ledger::standing::edited!(@ $phase, $n, "")
    };
    (own $phase:tt, $n:expr) => {
        $crate::store::ledger::standing::edited!(@ $phase, $n,
            " AND NOT EXISTS (SELECT 1 FROM event_node oc JOIN node ok ON ok.id=oc.node WHERE \
             oc.event=eb.event AND oc.effect='changed' AND ok.kind IN ('flaw','path'))")
    };
    (@ $phase:tt, $n:expr, $own:expr) => {
        concat!(
            "(SELECT eb.event FROM ",
            $crate::store::ledger::bearing::bearing!(@stored $phase, $n),
            " AND eb.role='edits' AND eb.event IS NOT ",
            $crate::store::ledger::bearing::retirement!($n),
            $own,
            " ORDER BY eb.event DESC LIMIT 1)"
        )
    };
}
pub(in crate::store) use edited;
